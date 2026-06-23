//! OCR: client for the rsclaw-native `/v1/agent/ocr` endpoint.
//!
//! Autoregressive OCR (optionally bbox-annotated, streamable). The KB
//! image/scanned-PDF canonicalizer and the `ocr` agent tool both go
//! through here. We always request `stream:false` for a single JSON
//! `{content, finish_reason, usage}` reply (note: `content`, not `text`
//! — same as the vision lane).
//!
//! Transport mirrors `OpenAiEmbedder`/`KbReranker`: an async reqwest
//! client driven via `block_in_place` when a runtime is live, or a temp
//! runtime otherwise, so it's safe to call from both the async HTTP path
//! and the `spawn_blocking` tool path. Idle-pool reuse is disabled
//! (`build_remote_client`) for the same dead-socket reason embeddings hit.

use anyhow::{Context, Result};

/// OCR request deadline. A full page of dense CJK can take a while on an
/// autoregressive model; past this the endpoint is wedged.
const OCR_TIMEOUT_SECS: u64 = 120;

pub struct OcrClient {
    client: rsclaw_embed::FleetHttp,
    url: String,
    model: Option<String>,
    api_key: Option<String>,
    lang: Option<String>,
}

impl OcrClient {
    /// Build from the `kb.ocr` config block. Returns `None` when the
    /// block is absent or disabled. base_url may be empty when `model` is
    /// a `rsclaw-*` name (defaults to the fleet API).
    pub fn from_config() -> Option<std::sync::Arc<Self>> {
        let cfg = rsclaw_config::load().ok()?;
        let oc = cfg.raw.kb.as_ref()?.ocr.clone()?;
        if !oc.enabled.unwrap_or(true) {
            return None;
        }
        let model_is_rsclaw = oc
            .model
            .as_deref()
            .map(rsclaw_embed::is_rsclaw_model)
            .unwrap_or(false);
        let base_raw = oc.base_url.trim();
        let base = if base_raw.is_empty() {
            if model_is_rsclaw {
                rsclaw_embed::RSCLAW_API_BASE_URL.to_owned()
            } else {
                return None;
            }
        } else {
            base_raw.trim_end_matches('/').to_owned()
        };
        let api_key = oc
            .api_key
            .as_ref()
            .and_then(|s| s.resolve_early())
            .or_else(|| rsclaw_provider_key(&cfg));
        // OCR is an agent-lane capability (autoregressive, same lane as
        // /v1/agent/vision), so the path is `/agent/ocr` off the /v1 root —
        // NOT a top-level /v1/ocr. base resolves to `…/v1`, so append
        // `/agent/ocr`. (Verified live: /v1/agent/ocr → 503 no_worker,
        // /v1/ocr → 404 no route.)
        Some(std::sync::Arc::new(Self {
            // FleetHttp = redirect-cached fleet client (308 baseUrl caching +
            // idle-pool disabled), shared with the rsclaw provider and the
            // embed/rerank lanes so they all amortise the LB redirect.
            client: rsclaw_embed::FleetHttp::new(None),
            url: format!("{base}/agent/ocr"),
            model: oc.model,
            api_key,
            lang: oc.lang,
        }))
    }

    /// Whether an OCR endpoint is configured (cheap check for callers that
    /// want to branch before assembling an image payload).
    pub fn is_configured() -> bool {
        rsclaw_config::load()
            .ok()
            .and_then(|c| c.raw.kb.as_ref().and_then(|k| k.ocr.clone()))
            .map(|o| o.enabled.unwrap_or(true))
            .unwrap_or(false)
    }

    /// Run OCR on an image. `image` is a data URI (`data:image/png;base64,…`)
    /// or an `http(s)://` URL. Returns the extracted text (`content`).
    pub fn ocr(&self, image: &str) -> Result<String> {
        let mut body = serde_json::json!({
            "image": image,
            "stream": false,
        });
        if let Some(m) = &self.model {
            body["model"] = serde_json::json!(m);
        }
        if let Some(l) = &self.lang {
            body["lang"] = serde_json::json!(l);
        }

        let send = || async {
            let resp = self
                .client
                .post_following_redirects(
                    self.url.as_str(),
                    &body,
                    self.api_key.as_deref(),
                    false,
                    None,
                    Some(std::time::Duration::from_secs(OCR_TIMEOUT_SECS)),
                )
                .await?
                .error_for_status()?;
            anyhow::Ok(resp.json::<serde_json::Value>().await?)
        };
        let resp: serde_json::Value = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                tokio::task::block_in_place(|| handle.block_on(send())).context("ocr request failed")?
            }
            Err(_) => tokio::runtime::Runtime::new()
                .context("failed to create temp runtime for ocr")?
                .block_on(send())
                .context("ocr request failed")?,
        };

        // Native lane returns `content` (not `text`); tolerate both plus the
        // OpenAI-ish `choices[0].message.content` just in case.
        let content = resp
            .get("content")
            .and_then(|v| v.as_str())
            .or_else(|| resp.get("text").and_then(|v| v.as_str()))
            .or_else(|| {
                resp.pointer("/choices/0/message/content")
                    .and_then(|v| v.as_str())
            })
            .context("ocr response missing `content`")?;
        Ok(content.to_owned())
    }
}

/// Pull the rsclaw provider's API key for fleet calls (OCR, embed, rerank —
/// all first-party `rsclaw-*` models share this one key) so they can
/// authenticate without a dedicated per-feature `apiKey`.
///
/// Resolution mirrors the main rsclaw chat provider: an explicit
/// `models.providers.rsclaw.apiKey` if set, else the `RSCLAW_API_KEY` /
/// `RSCLAW_KEY` env vars. The common deployment has no per-provider apiKey in
/// config and relies entirely on the env var — without the env fallback here
/// rerank/embed/ocr sent no Bearer and the fleet 401'd ("missing
/// Authorization: Bearer header"), silently degrading KB rerank to fused order.
pub(crate) fn rsclaw_provider_key(
    cfg: &rsclaw_config::runtime::RuntimeConfig,
) -> Option<String> {
    cfg.raw
        .models
        .as_ref()
        .and_then(|m| m.providers.get("rsclaw"))
        .and_then(|p| p.api_key.as_ref())
        .and_then(|s| s.resolve_early())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("RSCLAW_API_KEY").ok().filter(|s| !s.is_empty()))
        .or_else(|| std::env::var("RSCLAW_KEY").ok().filter(|s| !s.is_empty()))
}

/// The configured `models.providers.rsclaw.base_url`, if non-empty. Used to
/// resolve an empty kb embed/rerank `base_url` for `rsclaw-*` models to the
/// rsclaw provider's own base (honouring a self-hosted fleet) instead of the
/// hardcoded `RSCLAW_API_BASE_URL` constant. The chat provider base may carry
/// the `/agent` protocol mount; embed/rerank live at the `/v1` root, so a
/// trailing `/agent` is stripped.
pub(crate) fn rsclaw_provider_base_url(
    cfg: &rsclaw_config::runtime::RuntimeConfig,
) -> Option<String> {
    cfg.raw
        .models
        .as_ref()
        .and_then(|m| m.providers.get("rsclaw"))
        .and_then(|p| p.base_url.as_ref())
        .map(|s| {
            let t = s.trim().trim_end_matches('/');
            t.strip_suffix("/agent").unwrap_or(t).trim_end_matches('/').to_owned()
        })
        .filter(|s| !s.is_empty())
}

impl std::fmt::Debug for OcrClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OcrClient")
            .field("url", &self.url)
            .field("model", &self.model)
            .finish()
    }
}
