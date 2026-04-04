//! Text-to-speech providers.
//!
//! Supported:
//!   - `openai` -- OpenAI TTS API
//!   - `system` -- macOS `say` / Linux `espeak`
//!   - `candle` -- future: local neural TTS via candle

use anyhow::Result;
use tracing::{info, warn};

/// Synthesize speech from text using the best available provider.
pub async fn text_to_speech(text: &str, client: &reqwest::Client) -> Result<Vec<u8>> {
    let provider = detect_tts_provider();
    info!(provider = %provider, chars = text.len(), "TTS request");
    match provider.as_str() {
        "openai" => tts_openai(text, client).await,
        "system" => tts_system(text).await,
        _ => anyhow::bail!("no TTS provider available"),
    }
}

fn detect_tts_provider() -> String {
    // Explicit override
    if let Ok(p) = std::env::var("TTS_PROVIDER") {
        return p.to_lowercase();
    }
    if std::env::var("OPENAI_API_KEY").is_ok() {
        return "openai".to_string();
    }
    "system".to_string()
}

async fn tts_openai(text: &str, client: &reqwest::Client) -> Result<Vec<u8>> {
    let api_key = std::env::var("OPENAI_API_KEY")?;
    let resp = client
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(&api_key)
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": text,
            "voice": "alloy",
            "response_format": "opus",
        }))
        .send()
        .await?
        .error_for_status()?;
    let bytes = resp.bytes().await?.to_vec();
    info!(bytes = bytes.len(), "OpenAI TTS complete");
    Ok(bytes)
}

async fn tts_system(text: &str) -> Result<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        let tmp_path = std::env::temp_dir().join("rsclaw_tts.aiff");
        let tmp_path_str = tmp_path.to_string_lossy().to_string();
        let output = tokio::process::Command::new("say")
            .args(["-o", &tmp_path_str, text])
            .output()
            .await?;
        if output.status.success() {
            let bytes = tokio::fs::read(&tmp_path).await?;
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Ok(bytes);
        }
        warn!("macOS say command failed");
    }

    #[cfg(target_os = "linux")]
    {
        let tmp_path = std::env::temp_dir().join("rsclaw_tts.wav");
        let tmp_path_str = tmp_path.to_string_lossy().to_string();
        let output = tokio::process::Command::new("espeak")
            .args(["-w", &tmp_path_str, text])
            .output()
            .await?;
        if output.status.success() {
            let bytes = tokio::fs::read(&tmp_path).await?;
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Ok(bytes);
        }
        warn!("Linux espeak command failed");
    }

    anyhow::bail!("system TTS not available on this platform")
}
