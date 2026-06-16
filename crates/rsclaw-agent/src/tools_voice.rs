//! Voice generation tool — `voice_gen` (`POST /v1/audio/speech`, gen-api.md §5).
//! High-quality text→speech with optional one-shot voice cloning. Synchronous;
//! the shared submit/save path lives in `tools_audio.rs`.
//!
//! Distinct from `text_to_voice` (the simple, free, offline local-OS read-aloud
//! tool) — cloning / high-quality is a separate capability, kept as its own
//! tool by design rather than overloaded onto the local TTS.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

impl super::runtime::AgentRuntime {
    /// Voice generation — text → speech, with optional one-shot voice clone
    /// (`reference_audio`). `text` required.
    pub(crate) async fn tool_voice(&self, args: Value) -> Result<Value> {
        let input = args["text"]
            .as_str()
            .or_else(|| args["input"].as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("voice_gen: `text` (the words to speak) is required"))?;
        let model = args["model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|m| m.rsplit('/').next().unwrap_or(m))
            .unwrap_or("rsclaw-voice-v1");
        let fmt = super::tools_audio::audio_format(&args);
        let mut body = json!({
            "model": model,
            "input": input,
            "response_format": fmt,
        });
        if let Some(voice) = args["voice"].as_str().filter(|s| !s.is_empty()) {
            body["voice"] = json!(voice);
        }
        if let Some(instr) = args["instructions"].as_str().filter(|s| !s.is_empty()) {
            body["instructions"] = json!(instr);
        }
        if let Some(speed) = args["speed"].as_f64() {
            body["speed"] = json!(speed);
        }
        // One-shot voice clone: reference_audio (URL / data-URI / local path →
        // base64). reference_text optionally improves fidelity.
        let refs = super::tools_video::normalize_gen_assets(&args["reference_audio"]).await;
        if let Some(r) = refs.first() {
            body["reference_audio"] = json!({ "audio_url": r });
            if let Some(rt) = args["reference_text"].as_str().filter(|s| !s.is_empty()) {
                body["reference_text"] = json!(rt);
            }
        }
        // The rsclaw voice backend REQUIRES a voice descriptor or a clone
        // reference — plain text-only synthesis 502s with "provide --voice-desc
        // or --ref-audio". When the caller gave neither, supply a sensible
        // default voice so a bare "read this aloud" still works.
        if body.get("voice").is_none() && body.get("reference_audio").is_none() {
            body["voice"] = json!("标准女声，自然清晰");
        }
        self.audio_submit("/v1/audio/speech", &body, &fmt, "voice")
            .await
    }
}
