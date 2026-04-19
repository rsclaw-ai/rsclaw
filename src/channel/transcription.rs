//! Multi-provider audio transcription.
//!
//! Supported providers:
//!   - `openai`   — OpenAI Whisper API (default)
//!   - `local`    — Local whisper.cpp via command-line
//!   - `tencent`  — Tencent Cloud ASR (腾讯语音识别)
//!   - `aliyun`   — Alibaba Cloud ASR (阿里语音识别)
//!   - `builtin`  — Platform's built-in recognition (e.g., WeChat)
//!
//! Audio decoding uses symphonia (pure Rust) — no ffmpeg required.
//!
//! Configuration via `memorySearch.provider` or env `TRANSCRIPTION_PROVIDER`.

use anyhow::{Context, Result};
use reqwest::Client;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Transcribe audio to text using the configured provider.
///
/// Provider resolution order:
/// 1. `TRANSCRIPTION_PROVIDER` env var
/// 2. Auto-detect: local whisper binary → OpenAI key → error
pub async fn transcribe_audio(
    client: &Client,
    audio_bytes: &[u8],
    file_name: &str,
    mime_type: &str,
) -> Result<String> {
    let is_silk = mime_type == "audio/silk"
        || file_name.ends_with(".silk")
        || audio_bytes.starts_with(b"\x02#!SILK")
        || audio_bytes.starts_with(b"#!SILK");

    // SILK -> PCM conversion (WeChat voice format)
    let (effective_bytes, effective_name, effective_mime);
    if is_silk {
        match decode_silk_to_wav(audio_bytes) {
            Ok(wav) => {
                info!(
                    silk_bytes = audio_bytes.len(),
                    wav_bytes = wav.len(),
                    "SILK decoded to WAV"
                );
                effective_bytes = wav;
                effective_name = "voice.wav".to_string();
                effective_mime = "audio/wav".to_string();
            }
            Err(e) => {
                warn!("SILK decode failed ({e:#}), trying raw");
                effective_bytes = audio_bytes.to_vec();
                effective_name = file_name.to_string();
                effective_mime = mime_type.to_string();
            }
        }
    } else {
        effective_bytes = audio_bytes.to_vec();
        effective_name = file_name.to_string();
        effective_mime = mime_type.to_string();
    }

    let provider = detect_provider();
    info!(provider = %provider, file = effective_name, bytes = effective_bytes.len(), "transcribing audio");

    match provider.as_str() {
        "candle" => transcribe_candle(&effective_bytes).await,
        "macos" => transcribe_macos(&effective_bytes, &effective_name).await,
        "local" => transcribe_local(&effective_bytes, &effective_name).await,
        "tencent" => transcribe_tencent(client, &effective_bytes, &effective_name).await,
        "aliyun" => transcribe_aliyun(client, &effective_bytes, &effective_name).await,
        _ => transcribe_openai(client, &effective_bytes, &effective_name, &effective_mime).await,
    }
}

/// Download a file from a URL and return the bytes.
pub async fn download_file(client: &Client, url: &str) -> Result<Vec<u8>> {
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("download failed: {}", resp.status());
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Resolve OpenAI API key from environment variable.
pub fn resolve_openai_key() -> Option<String> {
    if let Ok(key) = std::env::var("OPENAI_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    None
}

// ---------------------------------------------------------------------------
// SILK -> WAV conversion
// ---------------------------------------------------------------------------

/// Decode WeChat SILK v3 audio to 16-bit 16kHz mono WAV.
#[allow(unexpected_cfgs)]
fn decode_silk_to_wav(silk_bytes: &[u8]) -> Result<Vec<u8>> {
    // Strip WeChat \x02 prefix if present
    let _raw = if silk_bytes.first() == Some(&0x02) {
        &silk_bytes[1..]
    } else {
        silk_bytes
    };

    // Decode SILK to raw PCM bytes (16-bit samples at 24000 Hz)
    #[cfg(feature = "silk")]
    let pcm_bytes: Vec<u8> =
        silk_rs::decode_silk(raw, 24000).map_err(|e| anyhow::anyhow!("silk decode: {e:?}"))?;

    #[cfg(not(feature = "silk"))]
    {
        warn!("SILK decoding requires `silk` feature flag");
        return Err(anyhow::anyhow!(
            "SILK decoding not available: compile with --features silk"
        ));
    }

    #[cfg(feature = "silk")]
    {
        // Build WAV header (16-bit mono PCM)
        let sample_rate: u32 = 24000;
        let channels: u16 = 1;
        let bits_per_sample: u16 = 16;
        let data_len: u32 = pcm_bytes.len() as u32;
        let chunk_size: u32 = 36 + data_len;
        let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample) / 8;
        let block_align = channels * bits_per_sample / 8;

        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&chunk_size.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // chunk size
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM format
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits_per_sample.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend_from_slice(&pcm_bytes);

        Ok(wav)
    }
}

// ---------------------------------------------------------------------------
// Provider detection
// ---------------------------------------------------------------------------

fn detect_provider() -> String {
    if let Ok(p) = std::env::var("TRANSCRIPTION_PROVIDER") {
        return p.to_lowercase();
    }

    // 1. Candle whisper model (pure Rust, no external deps)
    let model_dir = crate::config::loader::base_dir().join("models/whisper-tiny");
    if model_dir.join("config.json").exists() {
        return "candle".to_owned();
    }

    // 2. Local whisper binary + model file (fastest, free, offline)
    if which_whisper().is_some() && find_whisper_model().is_some() {
        return "local".to_owned();
    }

    // 3. macOS built-in SFSpeechRecognizer (requires permission grant in System
    //    Settings)
    #[cfg(target_os = "macos")]
    if std::env::var("TRANSCRIPTION_MACOS").is_ok() {
        return "macos".to_owned();
    }

    // 4. Cloud providers
    if std::env::var("TENCENT_SECRET_ID").is_ok() {
        return "tencent".to_owned();
    }
    if std::env::var("ALIYUN_ACCESS_KEY_ID").is_ok() {
        return "aliyun".to_owned();
    }

    // 5. OpenAI Whisper API (needs valid API key)
    if resolve_openai_key().is_some() {
        return "openai".to_owned();
    }

    // 6. Last resort: try local whisper even without verified model
    if which_whisper().is_some() {
        return "local".to_owned();
    }

    "openai".to_owned()
}

// ---------------------------------------------------------------------------
// macOS native (SFSpeechRecognizer)
// ---------------------------------------------------------------------------

/// Transcribe using macOS built-in Speech Recognition.
/// Uses SFSpeechRecognizer via a small Swift script — works offline,
/// supports Chinese, English, Japanese, and many other languages.
/// No API key needed.
async fn transcribe_macos(audio_bytes: &[u8], file_name: &str) -> Result<String> {
    let tmp_dir = std::env::temp_dir();

    // Write audio to temp file
    let audio_path = tmp_dir.join(format!("rsclaw_voice_{}", file_name));
    std::fs::write(&audio_path, audio_bytes)?;

    // Convert to 16kHz mono WAV using symphonia (pure Rust, no ffmpeg needed).
    // SFSpeechRecognizer works best with WAV/M4A formats.
    let wav_path = tmp_dir.join(format!("rsclaw_voice_{}.wav", uuid::Uuid::new_v4()));
    let ext = file_name.rsplit('.').next();
    let input = match decode_audio_to_pcm_ext(audio_bytes, ext) {
        Ok(pcm) => {
            let wav_data = write_wav_from_pcm(&pcm, 16000);
            std::fs::write(&wav_path, &wav_data)?;
            wav_path.clone()
        }
        Err(e) => {
            warn!("symphonia decode failed ({e:#}), trying afconvert");
            let convert_ok = tokio::process::Command::new("afconvert")
                .args([
                    "-f", "WAVE", "-d", "LEI16@16000",
                    audio_path.to_str().unwrap_or(""),
                    wav_path.to_str().unwrap_or(""),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false);
            if convert_ok {
                wav_path.clone()
            } else {
                audio_path.clone()
            }
        }
    };

    // Swift script that uses SFSpeechRecognizer
    let swift_script = r#"
import Foundation
import Speech

let args = CommandLine.arguments
guard args.count > 1 else { exit(1) }
let url = URL(fileURLWithPath: args[1])

SFSpeechRecognizer.requestAuthorization { status in
    guard status == .authorized else {
        fputs("Speech recognition not authorized\n", stderr)
        exit(2)
    }
}

let recognizer = SFSpeechRecognizer(locale: Locale(identifier: "zh-Hans"))
    ?? SFSpeechRecognizer()!
let request = SFSpeechURLRecognitionRequest(url: url)
request.shouldReportPartialResults = false

let semaphore = DispatchSemaphore(value: 0)
var resultText = ""

recognizer.recognitionTask(with: request) { result, error in
    if let error = error {
        fputs("Recognition error: \(error.localizedDescription)\n", stderr)
        semaphore.signal()
        return
    }
    if let result = result, result.isFinal {
        resultText = result.bestTranscription.formattedString
        semaphore.signal()
    }
}

_ = semaphore.wait(timeout: .now() + 30)
print(resultText)
"#;

    let script_path = tmp_dir.join("rsclaw_speech_recognizer.swift");
    std::fs::write(&script_path, swift_script)?;

    let output = tokio::process::Command::new("swift")
        .arg(&script_path)
        .arg(&input)
        .output()
        .await
        .context("failed to run Swift speech recognizer")?;

    // Cleanup
    let _ = std::fs::remove_file(&audio_path);
    let _ = std::fs::remove_file(&wav_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Fall back to OpenAI if macOS recognition fails
        warn!("macOS speech recognition failed: {stderr}, falling back to OpenAI");
        let audio = std::fs::read(&audio_path).unwrap_or_else(|_| audio_bytes.to_vec());
        return transcribe_openai(&reqwest::Client::new(), &audio, file_name, "audio/ogg").await;
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if text.is_empty() {
        warn!("macOS speech recognition returned empty, falling back to OpenAI");
        return transcribe_openai(&reqwest::Client::new(), audio_bytes, file_name, "audio/ogg")
            .await;
    }

    debug!(chars = text.len(), "macOS native transcription complete");
    Ok(text)
}

// ---------------------------------------------------------------------------
// OpenAI Whisper
// ---------------------------------------------------------------------------

async fn transcribe_openai(
    client: &Client,
    audio_bytes: &[u8],
    file_name: &str,
    mime_type: &str,
) -> Result<String> {
    let api_key = resolve_openai_key()
        .context("OpenAI API key needed for voice transcription (set OPENAI_API_KEY or use TRANSCRIPTION_PROVIDER=local)")?;

    let part = reqwest::multipart::Part::bytes(audio_bytes.to_vec())
        .file_name(file_name.to_owned())
        .mime_str(mime_type)?;

    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);

    let resp = client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(&api_key)
        .multipart(form)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Whisper API error {status}: {body}");
    }

    let result: serde_json::Value = resp.json().await?;
    let text = result
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();

    debug!(chars = text.len(), "OpenAI Whisper transcription complete");
    Ok(text)
}

// ---------------------------------------------------------------------------
// Local Whisper (whisper.cpp CLI)
// ---------------------------------------------------------------------------

/// Find whisper binary: `whisper-cli`, `whisper`, or `whisper.cpp`
fn which_whisper() -> Option<String> {
    // 1. Check ~/.rsclaw/tools/whisper-cpp/ first
    let tools_dir = crate::config::loader::base_dir().join("tools/whisper-cpp");
    if tools_dir.exists() {
        for name in &["whisper-cli", "whisper", "main"] {
            let bin = tools_dir.join(name);
            if bin.exists() {
                return Some(bin.to_string_lossy().to_string());
            }
        }
    }

    // 2. System PATH
    for name in &["whisper-cli", "whisper", "whisper-cpp", "main"] {
        if let Ok(path) = which::which(name) {
            return Some(path.to_string_lossy().to_string());
        }
    }
    None
}

/// Find a whisper model file. Checks WHISPER_MODEL env, then common paths.
fn find_whisper_model() -> Option<String> {
    // Explicit env var (can be a name like "base" or a full path)
    if let Ok(m) = std::env::var("WHISPER_MODEL") {
        if std::path::Path::new(&m).exists() {
            return Some(m);
        }
        // Try as a model name in common locations
        #[cfg(not(windows))]
        const MODEL_DIRS: &[&str] = &[
            "/opt/homebrew/share/whisper-cpp/models",
            "/usr/local/share/whisper-cpp/models",
        ];
        #[cfg(windows)]
        const MODEL_DIRS: &[&str] = &[
            "C:\\ProgramData\\whisper-cpp\\models",
            "C:\\whisper-cpp\\models",
        ];
        for dir in MODEL_DIRS {
            let path = format!("{dir}/ggml-{m}.bin");
            if std::path::Path::new(&path).exists() {
                return Some(path);
            }
        }
        // If it's a name but no file found, return None
        return None;
    }

    // Search common model locations
    #[cfg(not(windows))]
    const SEARCH_DIRS: &[&str] = &[
        "/opt/homebrew/share/whisper-cpp/models",
        "/opt/homebrew/share/whisper-cpp",
        "/usr/local/share/whisper-cpp/models",
        "/usr/local/share/whisper-cpp",
    ];
    #[cfg(windows)]
    const SEARCH_DIRS: &[&str] = &[
        "C:\\ProgramData\\whisper-cpp\\models",
        "C:\\ProgramData\\whisper-cpp",
        "C:\\whisper-cpp\\models",
        "C:\\whisper-cpp",
    ];
    for dir in SEARCH_DIRS {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("ggml-") && name.ends_with(".bin") {
                    return Some(entry.path().to_string_lossy().to_string());
                }
            }
        }
    }

    // Check home directory cache
    if let Some(home) = dirs_next::home_dir() {
        let cache = home.join(".cache/whisper");
        if let Ok(entries) = std::fs::read_dir(&cache) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".bin") || name.ends_with(".gguf") {
                    return Some(entry.path().to_string_lossy().to_string());
                }
            }
        }
    }

    None
}

async fn transcribe_local(audio_bytes: &[u8], file_name: &str) -> Result<String> {
    let whisper_bin = which_whisper()
        .context("whisper CLI not found. Run `rsclaw tools install whisper-cpp`, download from https://gitfast.io, or `brew install whisper-cpp`")?;

    // Write audio to temp file
    let tmp_dir = std::env::temp_dir();
    let audio_path = tmp_dir.join(format!("rsclaw_voice_{}", file_name));
    std::fs::write(&audio_path, audio_bytes)?;

    // Convert to WAV 16kHz mono using symphonia (pure Rust, no ffmpeg needed)
    let wav_path = tmp_dir.join(format!("rsclaw_voice_{}.wav", uuid::Uuid::new_v4()));
    let ext = file_name.rsplit('.').next();
    let input_path = match decode_audio_to_pcm_ext(audio_bytes, ext) {
        Ok(pcm) => {
            let wav_data = write_wav_from_pcm(&pcm, 16000);
            std::fs::write(&wav_path, &wav_data)?;
            &wav_path
        }
        Err(e) => {
            warn!("symphonia decode failed ({e:#}), trying whisper with original file");
            &audio_path
        }
    };

    // Determine model path
    let model = find_whisper_model()
        .context("no whisper model found (download one or set WHISPER_MODEL=/path/to/model.bin)")?;

    // Run whisper
    // Use WHISPER_LANGUAGE env (default: "zh" for Chinese).
    // Set WHISPER_LANGUAGE=auto to let whisper auto-detect.
    let language = std::env::var("WHISPER_LANGUAGE").unwrap_or_else(|_| "zh".to_owned());
    let mut args = vec![
        "-m".to_owned(),
        model,
        "-f".to_owned(),
        input_path.to_str().unwrap_or("input").to_owned(),
        "--no-timestamps".to_owned(),
        "--language".to_owned(),
        language.clone(),
    ];
    // For Chinese: add initial prompt to guide simplified Chinese output
    if language == "zh" {
        args.push("--prompt".to_owned());
        args.push("以下是普通话的句子。".to_owned());
    }
    let convert_to_simplified = language == "zh";

    debug!(cmd = %whisper_bin, args = ?args, "running whisper");
    let output = tokio::process::Command::new(&whisper_bin)
        .args(&args)
        .output()
        .await
        .context("failed to run whisper")?;

    // Cleanup temp files
    let _ = std::fs::remove_file(&audio_path);
    let _ = std::fs::remove_file(&wav_path);

    let stderr_str = String::from_utf8_lossy(&output.stderr);
    if !stderr_str.is_empty() {
        debug!(
            "whisper stderr: {}",
            &stderr_str[..stderr_str.len().min(500)]
        );
    }

    if !output.status.success() {
        anyhow::bail!("whisper failed: {stderr_str}");
    }

    let mut text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if convert_to_simplified {
        text = traditional_to_simplified(&text);
    }
    info!(chars = text.len(), text = %text, "local whisper transcription complete");
    Ok(text)
}

/// Convert traditional Chinese characters to simplified Chinese.
/// Covers the ~500 most common traditional-simplified character pairs.
pub fn traditional_to_simplified(text: &str) -> String {
    // Pairs: (traditional, simplified)
    const PAIRS: &[(char, char)] = &[
        ('國', '国'),
        ('學', '学'),
        ('開', '开'),
        ('門', '门'),
        ('問', '问'),
        ('間', '间'),
        ('關', '关'),
        ('東', '东'),
        ('車', '车'),
        ('長', '长'),
        ('張', '张'),
        ('見', '见'),
        ('現', '现'),
        ('電', '电'),
        ('話', '话'),
        ('說', '说'),
        ('請', '请'),
        ('讓', '让'),
        ('認', '认'),
        ('記', '记'),
        ('許', '许'),
        ('論', '论'),
        ('設', '设'),
        ('試', '试'),
        ('語', '语'),
        ('調', '调'),
        ('課', '课'),
        ('誰', '谁'),
        ('謝', '谢'),
        ('識', '识'),
        ('議', '议'),
        ('護', '护'),
        ('變', '变'),
        ('譯', '译'),
        ('讀', '读'),
        ('響', '响'),
        ('機', '机'),
        ('經', '经'),
        ('從', '从'),
        ('雙', '双'),
        ('發', '发'),
        ('對', '对'),
        ('時', '时'),
        ('動', '动'),
        ('萬', '万'),
        ('書', '书'),
        ('買', '买'),
        ('費', '费'),
        ('賣', '卖'),
        ('實', '实'),
        ('寫', '写'),
        ('導', '导'),
        ('將', '将'),
        ('專', '专'),
        ('層', '层'),
        ('幫', '帮'),
        ('廣', '广'),
        ('應', '应'),
        ('強', '强'),
        ('歲', '岁'),
        ('歷', '历'),
        ('歸', '归'),
        ('當', '当'),
        ('後', '后'),
        ('點', '点'),
        ('熱', '热'),
        ('燈', '灯'),
        ('愛', '爱'),
        ('產', '产'),
        ('畫', '画'),
        ('異', '异'),
        ('節', '节'),
        ('紅', '红'),
        ('約', '约'),
        ('給', '给'),
        ('統', '统'),
        ('經', '经'),
        ('結', '结'),
        ('練', '练'),
        ('線', '线'),
        ('緊', '紧'),
        ('續', '续'),
        ('總', '总'),
        ('綠', '绿'),
        ('網', '网'),
        ('義', '义'),
        ('習', '习'),
        ('聽', '听'),
        ('職', '职'),
        ('腦', '脑'),
        ('與', '与'),
        ('舊', '旧'),
        ('華', '华'),
        ('處', '处'),
        ('號', '号'),
        ('蘭', '兰'),
        ('術', '术'),
        ('衛', '卫'),
        ('裝', '装'),
        ('複', '复'),
        ('親', '亲'),
        ('觀', '观'),
        ('計', '计'),
        ('訂', '订'),
        ('訊', '讯'),
        ('訓', '训'),
        ('質', '质'),
        ('貝', '贝'),
        ('資', '资'),
        ('賓', '宾'),
        ('運', '运'),
        ('過', '过'),
        ('達', '达'),
        ('選', '选'),
        ('還', '还'),
        ('進', '进'),
        ('連', '连'),
        ('遠', '远'),
        ('適', '适'),
        ('遲', '迟'),
        ('邊', '边'),
        ('邏', '逻'),
        ('郵', '邮'),
        ('鄰', '邻'),
        ('醫', '医'),
        ('錢', '钱'),
        ('錯', '错'),
        ('鍵', '键'),
        ('鐘', '钟'),
        ('鑰', '钥'),
        ('陽', '阳'),
        ('陰', '阴'),
        ('隊', '队'),
        ('階', '阶'),
        ('際', '际'),
        ('難', '难'),
        ('雲', '云'),
        ('離', '离'),
        ('電', '电'),
        ('響', '响'),
        ('頁', '页'),
        ('頭', '头'),
        ('題', '题'),
        ('類', '类'),
        ('顯', '显'),
        ('風', '风'),
        ('飛', '飞'),
        ('飯', '饭'),
        ('體', '体'),
        ('髮', '发'),
        ('魚', '鱼'),
        ('鳥', '鸟'),
        ('麗', '丽'),
        ('麼', '么'),
        ('齊', '齐'),
        ('齒', '齿'),
        ('龍', '龙'),
        ('龜', '龟'),
        // Common compound usage chars
        ('辦', '办'),
        ('報', '报'),
        ('備', '备'),
        ('幣', '币'),
        ('標', '标'),
        ('補', '补'),
        ('參', '参'),
        ('側', '侧'),
        ('測', '测'),
        ('場', '场'),
        ('稱', '称'),
        ('傳', '传'),
        ('創', '创'),
        ('純', '纯'),
        ('詞', '词'),
        ('帶', '带'),
        ('單', '单'),
        ('擔', '担'),
        ('檔', '档'),
        ('島', '岛'),
        ('歡', '欢'),
        ('環', '环'),
        ('換', '换'),
        ('積', '积'),
        ('極', '极'),
        ('濟', '济'),
        ('繼', '继'),
        ('價', '价'),
        ('檢', '检'),
        ('簡', '简'),
        ('獎', '奖'),
        ('講', '讲'),
        ('將', '将'),
        ('區', '区'),
        ('確', '确'),
        ('勝', '胜'),
        ('聯', '联'),
        ('臨', '临'),
        ('領', '领'),
        ('滿', '满'),
        ('貿', '贸'),
        ('內', '内'),
        ('農', '农'),
        ('濃', '浓'),
        ('盤', '盘'),
        ('評', '评'),
        ('齊', '齐'),
        ('遷', '迁'),
        ('簽', '签'),
        ('權', '权'),
        ('勸', '劝'),
        ('讓', '让'),
        ('軟', '软'),
        ('傷', '伤'),
        ('聯', '联'),
        ('歲', '岁'),
        ('損', '损'),
        ('態', '态'),
        ('團', '团'),
        ('網', '网'),
        ('務', '务'),
        ('無', '无'),
        ('獻', '献'),
        ('鄉', '乡'),
        ('響', '响'),
        ('協', '协'),
        ('壓', '压'),
        ('鹽', '盐'),
        ('樣', '样'),
        ('業', '业'),
        ('藝', '艺'),
        ('優', '优'),
        ('園', '园'),
        ('閱', '阅'),
        ('雜', '杂'),
        ('戰', '战'),
        ('佔', '占'),
        ('針', '针'),
        ('陣', '阵'),
        ('鎮', '镇'),
        ('種', '种'),
        ('眾', '众'),
        ('莊', '庄'),
        ('準', '准'),
        ('組', '组'),
    ];

    let mut result = String::with_capacity(text.len());
    for ch in text.chars() {
        let simplified = PAIRS
            .iter()
            .find(|(t, _)| *t == ch)
            .map(|(_, s)| *s)
            .unwrap_or(ch);
        result.push(simplified);
    }
    result
}

// ---------------------------------------------------------------------------
// Tencent Cloud ASR (腾讯语音识别)
// ---------------------------------------------------------------------------

async fn transcribe_tencent(
    client: &Client,
    audio_bytes: &[u8],
    _file_name: &str,
) -> Result<String> {
    let secret_id = std::env::var("TENCENT_SECRET_ID").context("TENCENT_SECRET_ID not set")?;
    let secret_key = std::env::var("TENCENT_SECRET_KEY").context("TENCENT_SECRET_KEY not set")?;

    // Tencent ASR one-sentence recognition API
    // https://cloud.tencent.com/document/api/1093/37823
    let audio_b64 = base64_encode(audio_bytes);
    let body = serde_json::json!({
        "ProjectId": 0,
        "SubServiceType": 2,
        "EngSerViceType": "16k_zh",  // 16kHz Chinese
        "SourceType": 1,             // audio data in body
        "Data": audio_b64,
        "DataLen": audio_bytes.len(),
        "VoiceFormat": "ogg",
    });

    // Simplified call — in production, use proper TC3-HMAC-SHA256 signing.
    // For now, use the simple API with SecretId/SecretKey in headers.
    let resp = client
        .post("https://asr.tencentcloudapi.com/")
        .header("X-TC-Action", "SentenceRecognition")
        .header("X-TC-Version", "2019-06-14")
        .header("X-TC-Region", "ap-guangzhou")
        .header("X-TC-SecretId", &secret_id)
        .header("X-TC-SecretKey", &secret_key)
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(&body)?)
        .send()
        .await?;

    let result: serde_json::Value = resp.json().await?;
    let text = result
        .pointer("/Response/Result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();

    if text.is_empty() {
        let err = result
            .pointer("/Response/Error/Message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("Tencent ASR error: {err}");
    }

    debug!(chars = text.len(), "Tencent ASR transcription complete");
    Ok(text)
}

// ---------------------------------------------------------------------------
// Alibaba Cloud ASR (阿里语音识别)
// ---------------------------------------------------------------------------

async fn transcribe_aliyun(
    client: &Client,
    audio_bytes: &[u8],
    _file_name: &str,
) -> Result<String> {
    let _access_key =
        std::env::var("ALIYUN_ACCESS_KEY_ID").context("ALIYUN_ACCESS_KEY_ID not set")?;
    let app_key = std::env::var("ALIYUN_ASR_APP_KEY").context("ALIYUN_ASR_APP_KEY not set")?;

    // Get token first
    let token = std::env::var("ALIYUN_ASR_TOKEN").unwrap_or_default();

    // Alibaba Cloud one-sentence recognition
    // https://help.aliyun.com/document_detail/92131.html
    let resp = client
        .post("https://nls-gateway-cn-shanghai.aliyuncs.com/stream/v1/asr")
        .query(&[
            ("appkey", app_key.as_str()),
            ("format", "ogg"),
            ("sample_rate", "16000"),
        ])
        .header("X-NLS-Token", &token)
        .header("Content-Type", "application/octet-stream")
        .body(audio_bytes.to_vec())
        .send()
        .await?;

    let result: serde_json::Value = resp.json().await?;
    let text = result
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();

    if text.is_empty() {
        let msg = result
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let status = result.get("status").and_then(|v| v.as_i64()).unwrap_or(-1);
        if status != 20000000 {
            anyhow::bail!("Aliyun ASR error (status {status}): {msg}");
        }
    }

    debug!(chars = text.len(), "Aliyun ASR transcription complete");
    Ok(text)
}

// ---------------------------------------------------------------------------
// Audio decoding (symphonia — pure Rust, replaces ffmpeg)
// ---------------------------------------------------------------------------

/// Decode audio bytes to mono f32 PCM at 16 kHz.
/// Priority: OGG Opus → symphonia → ffmpeg fallback.
fn decode_audio_to_pcm(audio_bytes: &[u8]) -> Result<Vec<f32>> {
    decode_audio_to_pcm_ext(audio_bytes, None)
}

/// Same as decode_audio_to_pcm but with optional filename extension hint.
fn decode_audio_to_pcm_ext(audio_bytes: &[u8], file_ext: Option<&str>) -> Result<Vec<f32>> {
    // Try OGG Opus first (Telegram/Feishu/WhatsApp voice messages)
    if audio_bytes.starts_with(b"OggS") {
        if let Ok(samples) = decode_ogg_opus(audio_bytes) {
            return Ok(samples);
        }
    }
    // Detect format hint for symphonia
    let hint = file_ext
        .or_else(|| {
            if audio_bytes.len() >= 12 && audio_bytes[4..].windows(4).any(|w| w == b"ftyp") {
                Some("mp4")
            } else if audio_bytes.starts_with(b"\x1aE\xdf\xa3") {
                Some("webm")
            } else {
                None
            }
        });
    // Try symphonia (MP3/AAC/WAV/FLAC/MP4/MKV)
    if let Ok(samples) = decode_audio_symphonia_with_hint(audio_bytes, hint) {
        return Ok(samples);
    }
    // Fallback: ffmpeg (handles everything else)
    let ext = hint.or(file_ext).unwrap_or("bin");
    decode_audio_ffmpeg(audio_bytes, ext)
}

/// Resolve ffmpeg binary: ~/.rsclaw/tools/ffmpeg/ first, then system PATH.
fn which_ffmpeg() -> String {
    let tools_bin = crate::config::loader::base_dir().join("tools/ffmpeg/ffmpeg");
    if tools_bin.exists() {
        return tools_bin.to_string_lossy().to_string();
    }
    #[cfg(target_os = "windows")]
    {
        let tools_exe = crate::config::loader::base_dir().join("tools/ffmpeg/ffmpeg.exe");
        if tools_exe.exists() {
            return tools_exe.to_string_lossy().to_string();
        }
    }
    "ffmpeg".to_owned()
}

/// Fallback: use ffmpeg CLI to convert any audio/video to 16kHz mono WAV, then read PCM.
fn decode_audio_ffmpeg(audio_bytes: &[u8], ext: &str) -> Result<Vec<f32>> {
    let tmp_dir = std::env::temp_dir();
    let id = uuid::Uuid::new_v4();
    let input_path = tmp_dir.join(format!("rsclaw_ff_{id}.{ext}"));
    let wav_path = tmp_dir.join(format!("rsclaw_ff_{id}.wav"));

    std::fs::write(&input_path, audio_bytes)?;
    let ffmpeg_bin = which_ffmpeg();
    info!(input = %input_path.display(), ext = %ext, bytes = audio_bytes.len(), bin = %ffmpeg_bin, "ffmpeg fallback: converting");
    let output = std::process::Command::new(&ffmpeg_bin)
        .args([
            "-i", input_path.to_str().unwrap_or(""),
            "-ar", "16000", "-ac", "1", "-f", "wav",
            "-y", wav_path.to_str().unwrap_or(""),
        ])
        .output();
    let _ = std::fs::remove_file(&input_path);
    let status = output.as_ref().map(|o| o.status).map_err(|e| anyhow::anyhow!("{e}"));
    if let Ok(ref o) = output {
        let stderr = String::from_utf8_lossy(&o.stderr);
        warn!("ffmpeg exit={} stderr_tail: {}", o.status, stderr.chars().rev().take(300).collect::<String>().chars().rev().collect::<String>());
    }

    match status {
        Ok(s) if s.success() => {
            let wav_bytes = std::fs::read(&wav_path)?;
            let _ = std::fs::remove_file(&wav_path);
            // Parse WAV: skip 44-byte header, read i16 samples as f32
            if wav_bytes.len() > 44 {
                let samples: Vec<f32> = wav_bytes[44..]
                    .chunks(2)
                    .map(|c| {
                        let s = i16::from_le_bytes([c[0], c.get(1).copied().unwrap_or(0)]);
                        s as f32 / 32768.0
                    })
                    .collect();
                info!(samples = samples.len(), "ffmpeg fallback: decoded to 16kHz PCM");
                Ok(samples)
            } else {
                anyhow::bail!("ffmpeg: WAV output too short")
            }
        }
        _ => {
            let _ = std::fs::remove_file(&wav_path);
            anyhow::bail!("ffmpeg not available or failed. Install ffmpeg for full format support.")
        }
    }
}

/// Decode OGG Opus audio to mono f32 PCM at 16 kHz.
fn decode_ogg_opus(audio_bytes: &[u8]) -> Result<Vec<f32>> {
    use ogg::reading::PacketReader;

    let cursor = std::io::Cursor::new(audio_bytes);
    let mut reader = PacketReader::new(cursor);
    let mut decoder: Option<opus_decoder::OpusDecoder> = None;
    let mut samples: Vec<f32> = Vec::new();
    let mut channels = 1usize;

    while let Some(packet) = reader.read_packet()? {
        if decoder.is_none() {
            // First packet is the Opus header
            if packet.data.len() >= 12 && &packet.data[..8] == b"OpusHead" {
                channels = packet.data[9] as usize;
                if channels == 0 { channels = 1; }
                decoder = Some(opus_decoder::OpusDecoder::new(48000, channels)
                    .map_err(|e| anyhow::anyhow!("opus decoder init: {e}"))?);
            }
            continue;
        }
        // Skip comment packet
        if packet.data.starts_with(b"OpusTags") {
            continue;
        }
        // Decode audio packet
        if let Some(ref mut dec) = decoder {
            let max_samples = dec.max_frame_size_per_channel() * channels;
            let mut pcm = vec![0f32; max_samples];
            match dec.decode_float(&packet.data, &mut pcm, false) {
                Ok(n) => {
                    let total = n * channels;
                    // Mix to mono if stereo
                    if channels > 1 {
                        for chunk in pcm[..total].chunks(channels) {
                            samples.push(chunk.iter().sum::<f32>() / channels as f32);
                        }
                    } else {
                        samples.extend_from_slice(&pcm[..total]);
                    }
                }
                Err(_) => continue,
            }
        }
    }

    if samples.is_empty() {
        anyhow::bail!("opus: no samples decoded");
    }

    // Resample from 48kHz to 16kHz (Opus always outputs at 48kHz)
    {
        let ratio = 16000.0 / 48000.0_f64;
        let new_len = (samples.len() as f64 * ratio) as usize;
        let mut resampled = Vec::with_capacity(new_len);
        for i in 0..new_len {
            let src_pos = i as f64 / ratio;
            let idx = src_pos as usize;
            let frac = (src_pos - idx as f64) as f32;
            let s = if idx + 1 < samples.len() {
                samples[idx] * (1.0 - frac) + samples[idx + 1] * frac
            } else if idx < samples.len() {
                samples[idx]
            } else {
                0.0
            };
            resampled.push(s);
        }
        samples = resampled;
    }

    info!(samples = samples.len(), "opus: decoded to 16kHz PCM");
    Ok(samples)
}

/// Decode non-Opus audio (MP3, AAC, WAV, OGG Vorbis, FLAC) via symphonia.
fn decode_audio_symphonia_with_hint(audio_bytes: &[u8], ext_hint: Option<&str>) -> Result<Vec<f32>> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let cursor = std::io::Cursor::new(audio_bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = ext_hint {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("failed to probe audio format")?;

    let mut format = probed.format;
    let track = format.default_track().context("no audio track found")?;
    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(16000);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("failed to create audio decoder")?;

    let mut samples: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) if p.track_id() == track_id => p,
            Ok(_) => continue,
            Err(_) => break,
        };

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let spec = *decoded.spec();
        let num_samples = decoded.capacity();
        let mut sample_buf = SampleBuffer::<f32>::new(num_samples as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);

        let channel_samples = sample_buf.samples();
        let channels = spec.channels.count();

        // Mix to mono if stereo/multi-channel
        if channels > 1 {
            for chunk in channel_samples.chunks(channels) {
                let avg = chunk.iter().sum::<f32>() / channels as f32;
                samples.push(avg);
            }
        } else {
            samples.extend_from_slice(channel_samples);
        }
    }

    // Resample to 16 kHz if needed (linear interpolation)
    if sample_rate != 16000 && sample_rate > 0 {
        let ratio = 16000.0 / sample_rate as f64;
        let new_len = (samples.len() as f64 * ratio) as usize;
        let mut resampled = Vec::with_capacity(new_len);
        for i in 0..new_len {
            let src_pos = i as f64 / ratio;
            let idx = src_pos as usize;
            let frac = src_pos - idx as f64;
            let s = if idx + 1 < samples.len() {
                samples[idx] * (1.0 - frac as f32) + samples[idx + 1] * frac as f32
            } else if idx < samples.len() {
                samples[idx]
            } else {
                0.0
            };
            resampled.push(s);
        }
        samples = resampled;
    }

    Ok(samples)
}

/// Convert audio bytes (mp3/wav/ogg/aiff) to ogg-opus format.
///
/// Returns ogg-opus encoded bytes suitable for Feishu/Telegram voice messages.
/// Uses ffmpeg for encoding (opus-rs pure Rust produces incompatible output).
pub fn encode_audio_to_ogg_opus(audio_bytes: &[u8], file_ext: Option<&str>) -> Result<Vec<u8>> {
    let ext = file_ext.unwrap_or("mp3");
    let ts = chrono::Utc::now().timestamp_millis();
    let tmp_src = std::env::temp_dir().join(format!("rsclaw_opus_src_{ts}.{ext}"));
    let tmp_ogg = std::env::temp_dir().join(format!("rsclaw_opus_out_{ts}.ogg"));

    std::fs::write(&tmp_src, audio_bytes)?;
    let output = std::process::Command::new(which_ffmpeg())
        .args([
            "-i", &tmp_src.to_string_lossy(),
            "-y", "-c:a", "libopus", "-b:a", "48k",
            &tmp_ogg.to_string_lossy().to_string(),
        ])
        .output()
        .context("ffmpeg not available for opus encoding")?;
    let _ = std::fs::remove_file(&tmp_src);

    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp_ogg);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ffmpeg opus encoding failed: {stderr}");
    }

    let ogg_bytes = std::fs::read(&tmp_ogg)?;
    let _ = std::fs::remove_file(&tmp_ogg);
    info!(src_len = audio_bytes.len(), ogg_len = ogg_bytes.len(), "audio encoded to ogg-opus via ffmpeg");
    Ok(ogg_bytes)
}

/// Decode audio to mono f32 PCM at a specific target sample rate.
fn decode_audio_to_pcm_at_rate(
    audio_bytes: &[u8],
    file_ext: Option<&str>,
    target_rate: u32,
) -> Result<Vec<f32>> {
    // Try OGG Opus first
    if audio_bytes.starts_with(b"OggS") {
        if let Ok(samples) = decode_ogg_opus(audio_bytes) {
            // ogg-opus decodes at 48000 Hz
            return Ok(resample_linear(&samples, 48000, target_rate));
        }
    }
    // Detect format hint for symphonia
    let hint = file_ext.or_else(|| {
        if audio_bytes.len() >= 12 && audio_bytes[4..].windows(4).any(|w| w == b"ftyp") {
            Some("mp4")
        } else if audio_bytes.starts_with(b"\x1aE\xdf\xa3") {
            Some("webm")
        } else {
            None
        }
    });
    if let Ok((samples, rate)) = decode_audio_symphonia_raw(audio_bytes, hint) {
        return Ok(resample_linear(&samples, rate, target_rate));
    }
    // Fallback: ffmpeg
    let ext = hint.or(file_ext).unwrap_or("bin");
    let samples = decode_audio_ffmpeg_at_rate(audio_bytes, ext, target_rate)?;
    Ok(samples)
}

/// Linear interpolation resampling.
fn resample_linear(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || from_rate == 0 || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let new_len = (samples.len() as f64 * ratio) as usize;
    let mut out = Vec::with_capacity(new_len);
    for i in 0..new_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos as usize;
        let frac = src_pos - idx as f64;
        let s = if idx + 1 < samples.len() {
            samples[idx] * (1.0 - frac as f32) + samples[idx + 1] * frac as f32
        } else if idx < samples.len() {
            samples[idx]
        } else {
            0.0
        };
        out.push(s);
    }
    out
}

/// Symphonia decode returning (mono f32 samples, original sample rate).
fn decode_audio_symphonia_raw(audio_bytes: &[u8], ext_hint: Option<&str>) -> Result<(Vec<f32>, u32)> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let cursor = std::io::Cursor::new(audio_bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = ext_hint {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .context("failed to probe audio format")?;
    let mut format = probed.format;
    let track = format.default_track().context("no audio track found")?;
    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(16000);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("failed to create audio decoder")?;
    let mut samples: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) if p.track_id() == track_id => p,
            Ok(_) => continue,
            Err(_) => break,
        };
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let spec = *decoded.spec();
        let num_samples = decoded.capacity();
        let mut sample_buf = SampleBuffer::<f32>::new(num_samples as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        let channel_samples = sample_buf.samples();
        let channels = spec.channels.count();
        if channels > 1 {
            for chunk in channel_samples.chunks(channels) {
                let avg = chunk.iter().sum::<f32>() / channels as f32;
                samples.push(avg);
            }
        } else {
            samples.extend_from_slice(channel_samples);
        }
    }
    Ok((samples, sample_rate))
}

/// ffmpeg decode to f32 PCM at a specific sample rate.
fn decode_audio_ffmpeg_at_rate(audio_bytes: &[u8], ext: &str, target_rate: u32) -> Result<Vec<f32>> {
    let tmp_in = std::env::temp_dir().join(format!("rsclaw_in_{}.{ext}", chrono::Utc::now().timestamp_millis()));
    let tmp_out = std::env::temp_dir().join(format!("rsclaw_out_{}.pcm", chrono::Utc::now().timestamp_millis()));
    std::fs::write(&tmp_in, audio_bytes)?;
    let output = std::process::Command::new(which_ffmpeg())
        .args([
            "-i", &tmp_in.to_string_lossy(),
            "-y", "-f", "s16le", "-ar", &target_rate.to_string(), "-ac", "1",
            &tmp_out.to_string_lossy().to_string(),
        ])
        .output()
        .context("ffmpeg not available")?;
    let _ = std::fs::remove_file(&tmp_in);
    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp_out);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ffmpeg decode failed: {stderr}");
    }
    let pcm_bytes = std::fs::read(&tmp_out)?;
    let _ = std::fs::remove_file(&tmp_out);
    let samples: Vec<f32> = pcm_bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect();
    Ok(samples)
}

/// Write f32 PCM samples as a 16-bit mono WAV file.
fn write_wav_from_pcm(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample) / 8;
    let block_align = channels * bits_per_sample / 8;
    let data_len = (samples.len() * 2) as u32; // 2 bytes per 16-bit sample

    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());

    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let i16_val = (clamped * 32767.0) as i16;
        wav.extend_from_slice(&i16_val.to_le_bytes());
    }

    wav
}

// ---------------------------------------------------------------------------
// Candle Whisper (pure Rust — placeholder, requires model download)
// ---------------------------------------------------------------------------

/// Transcribe using candle-transformers whisper model (pure Rust, zero external deps).
///
/// Requires whisper-tiny model files at `<base_dir>/models/whisper-tiny/`:
///   - config.json, tokenizer.json, model.safetensors
///
/// Download: `huggingface-cli download openai/whisper-tiny --local-dir ~/.rsclaw/models/whisper-tiny`
async fn transcribe_candle(audio_bytes: &[u8]) -> Result<String> {
    let model_dir = crate::config::loader::base_dir().join("models/whisper-tiny");
    if !model_dir.join("config.json").exists() {
        anyhow::bail!(
            "candle whisper model not found at {}\n\
             Run: rsclaw models download whisper\n\
             Or download from: https://gitfast.io",
            model_dir.display()
        );
    }

    // Decode audio to 16 kHz mono PCM
    let pcm = decode_audio_to_pcm(audio_bytes)
        .context("failed to decode audio for candle whisper")?;

    info!(samples = pcm.len(), "decoded audio for candle whisper");

    // TODO: implement full candle whisper inference pipeline
    // For now, fall back to local whisper-cli if available, otherwise OpenAI
    if which_whisper().is_some() {
        warn!("candle whisper not yet fully implemented, falling back to local whisper-cli");
        // Write PCM as WAV for whisper-cli
        let wav_data = write_wav_from_pcm(&pcm, 16000);
        return transcribe_local(&wav_data, "voice.wav").await;
    }

    if resolve_openai_key().is_some() {
        warn!("candle whisper not yet fully implemented, falling back to OpenAI");
        return transcribe_openai(&reqwest::Client::new(), audio_bytes, "voice.ogg", "audio/ogg")
            .await;
    }

    anyhow::bail!(
        "candle whisper inference not yet implemented; \
         install whisper-cli or set OPENAI_API_KEY as fallback"
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn base64_encode(data: &[u8]) -> String {
    // Simple base64 encoding without external dependency.
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

