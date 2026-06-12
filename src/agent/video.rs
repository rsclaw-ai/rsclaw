//! Video processing helpers for the upload menu — ffmpeg-backed audio
//! extraction and keyframe sampling. Both are heavy, user-triggered
//! operations (menu options), never automatic.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// Extract the audio track as 16 kHz mono WAV bytes — the input format
/// `transcribe_audio` handles best. Errors when the video has no audio
/// stream (ffmpeg exits non-zero); callers surface that as "no audio".
pub async fn extract_audio_wav(ffmpeg: &str, video: &Path) -> Result<Vec<u8>> {
    let out = std::env::temp_dir().join(format!("rsclaw_vaudio_{}.wav", uuid::Uuid::new_v4()));
    let status = tokio::process::Command::new(ffmpeg)
        .args([
            "-y",
            "-i",
            &video.to_string_lossy(),
            "-vn",
            "-ar",
            "16000",
            "-ac",
            "1",
            "-f",
            "wav",
            &out.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .context("spawn ffmpeg for audio extraction")?;
    if !status.success() {
        let _ = std::fs::remove_file(&out);
        bail!("ffmpeg audio extraction failed (no audio track?)");
    }
    let bytes = std::fs::read(&out).context("read extracted wav")?;
    let _ = std::fs::remove_file(&out);
    if bytes.len() < 1024 {
        bail!("extracted audio is empty");
    }
    Ok(bytes)
}

/// Sample up to `max` keyframes as JPEGs (long edge capped at 960 px so a
/// handful of frames stays vision-budget friendly). Sampling rate adapts:
/// one frame every 10 seconds, which covers short clips densely and long
/// ones sparsely; ffmpeg stops emitting once the stream ends.
pub async fn extract_keyframes(ffmpeg: &str, video: &Path, max: usize) -> Result<Vec<PathBuf>> {
    let dir = std::env::temp_dir().join(format!("rsclaw_vframes_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).context("create frame dir")?;
    let pattern = dir.join("frame_%03d.jpg");
    let status = tokio::process::Command::new(ffmpeg)
        .args([
            "-y",
            "-i",
            &video.to_string_lossy(),
            "-vf",
            "fps=1/10,scale='min(960,iw)':-2",
            "-frames:v",
            &max.to_string(),
            "-q:v",
            "4",
            &pattern.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .context("spawn ffmpeg for keyframes")?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        bail!("ffmpeg keyframe extraction failed");
    }
    let mut frames: Vec<PathBuf> = std::fs::read_dir(&dir)
        .context("list frame dir")?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
        .collect();
    frames.sort();
    if frames.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(anyhow!("no keyframes produced"));
    }
    Ok(frames)
}

/// Best-effort cleanup of a keyframe batch (the shared temp parent dir).
pub fn cleanup_frames(frames: &[PathBuf]) {
    if let Some(dir) = frames.first().and_then(|f| f.parent()) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
