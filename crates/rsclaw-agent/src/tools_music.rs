//! Music generation tool — `music_gen` (`POST /v1/audio/music`, gen-api.md §4).
//! Style/lyrics → a song. Synchronous; the shared submit/save path lives in
//! `tools_audio.rs`.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

impl super::runtime::AgentRuntime {
    /// Music generation — style/lyrics → a song. `prompt` (style) required;
    /// `lyrics` / `duration` optional.
    pub(crate) async fn tool_music(&self, args: Value) -> Result<Value> {
        let prompt = args["prompt"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("music_gen: `prompt` (style description) is required"))?;
        let model = args["model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|m| m.rsplit('/').next().unwrap_or(m))
            .unwrap_or("rsclaw-music-v1");
        let fmt = super::tools_audio::audio_format(&args);
        let mut body = json!({
            "model": model,
            "prompt": prompt,
            "response_format": fmt,
        });
        if let Some(lyrics) = args["lyrics"].as_str().filter(|s| !s.is_empty()) {
            body["lyrics"] = json!(lyrics);
        }
        if let Some(dur) = args["duration"].as_u64() {
            body["duration"] = json!(dur);
        }
        self.audio_submit("/v1/audio/music", &body, &fmt, "music")
            .await
    }
}
