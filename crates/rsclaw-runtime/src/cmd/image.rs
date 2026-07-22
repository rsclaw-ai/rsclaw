//! CLI `rsclaw image {vision|ocr}` — directly call fleet vision / OCR endpoints.
//!
//! OCR reuses the existing `OcrClient`. Vision uses the same `FleetHttp`
//! infrastructure to target `/v1/agent/vision` (same pattern as OCR).

use anyhow::{Context, Result};
use base64::Engine as _;
use rsclaw_cli::ImageCommand;

use super::style::*;

const VISION_TIMEOUT_SECS: u64 = 120;

pub async fn cmd_image(sub: ImageCommand) -> Result<()> {
    match sub {
        ImageCommand::Vision(args) => {
            let data_uri = image_to_data_uri(&args.path)?;
            let model = resolve_vision_model(args.model.as_deref())?;
            let prompt = args
                .prompt
                .as_deref()
                .unwrap_or("Describe this image in detail.");

            let result = vision_describe(&data_uri, &model, prompt, args.max_tokens).await?;
            println!("{result}");
        }
        ImageCommand::Ocr(args) => {
            let data_uri = image_to_data_uri(&args.path)?;
            let client = rsclaw_kb::OcrClient::from_config()
                .ok_or_else(|| anyhow::anyhow!("OCR not configured; set kb.ocr in rsclaw.json5"))?;

            let result = client.ocr(&data_uri, args.prompt.as_deref(), args.max_tokens)?;
            println!("{result}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Encode a local image file as a data URI.
fn image_to_data_uri(path: &str) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read image: {path}"))?;
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes)))
}

/// Call `/v1/agent/vision` (same FleetHttp + redirect cache as OCR).
async fn vision_describe(
    image_data_uri: &str,
    model: &str,
    prompt: &str,
    max_tokens: Option<u32>,
) -> Result<String> {
    let cfg = rsclaw_config::load().context("load config")?;
    let base = resolve_vision_base(&cfg);
    let api_key = resolve_fleet_key(&cfg);

    let mut body = serde_json::json!({
        "model": model,
        "prompt": prompt,
        "images": [image_data_uri],
        "stream": false,
        "options": {
            "temperature": 0,
            "top_k": 1,
        },
    });
    if let Some(mt) = max_tokens {
        body["max_tokens"] = serde_json::json!(mt);
    }

    let send = || async {
        let client = rsclaw_embed::FleetHttp::new(None);
        let resp = client
            .post_following_redirects(
                &format!("{base}/agent/vision"),
                &body,
                api_key.as_deref(),
                false,
                None,
                Some(std::time::Duration::from_secs(VISION_TIMEOUT_SECS)),
            )
            .await?
            .error_for_status()?;
        anyhow::Ok(resp.json::<serde_json::Value>().await?)
    };

    let resp: serde_json::Value = match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(send()))
            .context("vision request failed")?,
        Err(_) => tokio::runtime::Runtime::new()
            .context("create temp runtime")?
            .block_on(send())
            .context("vision request failed")?,
    };

    let content = resp
        .get("content")
        .and_then(|v| v.as_str())
        .or_else(|| resp.get("text").and_then(|v| v.as_str()))
        .or_else(|| resp.pointer("/choices/0/message/content").and_then(|v| v.as_str()))
        .context("vision response missing `content`")?;
    Ok(content.to_owned())
}

/// Resolve the vision model: --model flag, then agents.defaults.model.vision,
/// then the rsclaw fleet default. Strips the `provider/` prefix (e.g.
/// `rsclaw/rsclaw-vision-v1` → `rsclaw-vision-v1`) so the fleet endpoint
/// receives the canonical bare model name.
fn resolve_vision_model(cli_model: Option<&str>) -> Result<String> {
    if let Some(m) = cli_model {
        return Ok(strip_provider_prefix(m));
    }
    if let Ok(cfg) = rsclaw_config::load() {
        if let Some(head) = cfg
            .raw
            .agents
            .as_ref()
            .and_then(|a| a.defaults.as_ref())
            .and_then(|d| d.model.as_ref())
            .and_then(|m| m.vision_head())
        {
            return Ok(strip_provider_prefix(head));
        }
    }
    Ok(strip_provider_prefix(
        rsclaw_provider::rsclaw::RSCLAW_DEFAULT_VISION,
    ))
}

/// Strip `provider/` prefix for fleet endpoints (e.g. `rsclaw/rsclaw-vision-v1` → `rsclaw-vision-v1`).
fn strip_provider_prefix(model: &str) -> String {
    model.rsplit_once('/').map(|(_, bare)| bare).unwrap_or(model).to_owned()
}

/// Resolve fleet base URL for vision, mirroring OcrClient's logic.
fn resolve_vision_base(cfg: &rsclaw_config::runtime::RuntimeConfig) -> String {
    // Try kb.ocr base_url first (same fleet).
    if let Some(ocr) = cfg
        .raw
        .kb
        .as_ref()
        .and_then(|k| k.ocr.as_ref())
        .filter(|o| o.enabled.unwrap_or(true))
    {
        let raw = ocr.base_url.trim();
        if !raw.is_empty() {
            return raw.trim_end_matches('/').to_owned();
        }
    }
    // Fallback: rsclaw provider base, stripping trailing `/agent`.
    if let Some(pbase) = cfg
        .raw
        .models
        .as_ref()
        .and_then(|m| m.providers.get("rsclaw"))
        .and_then(|p| p.base_url.as_ref())
        .map(|s| {
            let t = s.trim().trim_end_matches('/');
            t.strip_suffix("/agent").unwrap_or(t).trim_end_matches('/').to_owned()
        })
        .filter(|s| !s.is_empty())
    {
        return pbase;
    }
    rsclaw_embed::RSCLAW_API_BASE_URL.to_owned()
}

/// Resolve fleet API key, mirroring OcrClient.
fn resolve_fleet_key(cfg: &rsclaw_config::runtime::RuntimeConfig) -> Option<String> {
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
