//! OAuth2 QR code login for messaging platforms.
//!
//! Shared infrastructure for scan-to-login flows:
//!   1. Request auth URL / QR code from platform
//!   2. Display QR code in terminal
//!   3. Poll for user confirmation
//!   4. Store session token persistently
//!
//! Platform-specific implementations:
//!   - `feishu`  — Feishu/Lark OAuth2
//!   - `dingtalk` — DingTalk OAuth2

pub mod dingtalk_auth;
pub mod feishu_auth;

use anyhow::{Context, Result};
use qrcode::QrCode;
use tracing::info;

/// Display a URL as a QR code in the terminal.
///
/// Rendering strategy:
///   1. iTerm2 / WezTerm / Kitty — render as inline PNG image (pixel-perfect)
///   2. Other terminals — Unicode half-block characters (best-effort)
pub fn display_qr_terminal(url: &str) -> Result<()> {
    let code = QrCode::new(url.as_bytes()).context("failed to generate QR code")?;

    println!();

    // Try image-capable terminal first, then text fallback
    if !try_render_image(&code) {
        // Save temp PNG for fallback
        let png_path = save_qr_png(&code);

        // On Windows, open the PNG directly (terminal fonts distort Unicode QR).
        #[cfg(target_os = "windows")]
        if let Some(ref path) = png_path {
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", "", &path.display().to_string()])
                .spawn();
            println!("  QR code opened in image viewer.");
        }

        // Unicode half-block (compact, works on macOS Terminal and most UTF-8 terms)
        render_unicode(&code);

        if let Some(path) = png_path {
            println!("  If QR is distorted, open: file://{}", path.display());
        }
    }

    println!();
    println!("Scan the QR code above, or open this URL:");
    println!("  {url}\n");

    Ok(())
}

/// Render QR code as an inline PNG image using iTerm2/WezTerm/Kitty protocol.
///
/// iTerm2 inline image protocol:
///   ESC ] 1337 ; File=inline=1;size=N;width=auto : <base64 data> BEL
fn try_render_image(code: &QrCode) -> bool {
    // Check for image-capable terminal (not inside screen/tmux which can't forward)
    let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let is_iterm2 = term.contains("iTerm") || term.contains("WezTerm");
    let is_kitty = std::env::var("KITTY_PID").is_ok();
    let in_screen = std::env::var("STY").is_ok()
        || std::env::var("TERM")
            .ok()
            .is_some_and(|t| t.starts_with("screen"));
    let in_tmux = std::env::var("TMUX").is_ok();

    // screen/tmux can't reliably forward image protocols — skip
    if in_screen || in_tmux {
        return false;
    }

    if !is_iterm2 && !is_kitty {
        return false;
    }

    // Generate PNG in memory
    let png_data = match qr_to_png(code) {
        Some(data) => data,
        None => return false,
    };

    let b64 = base64_encode_bytes(&png_data);

    if is_kitty {
        print!("\x1b_Gf=100,a=T;{}\x1b\\", b64);
    } else {
        // iTerm2/WezTerm inline image
        print!(
            "\x1b]1337;File=inline=1;size={};width=auto;preserveAspectRatio=1:{}\x07",
            png_data.len(),
            b64
        );
    }
    println!();

    true
}

/// Generate a PNG byte buffer from a QrCode.
/// Each module = 8 pixels, with a 4-module quiet zone.
fn qr_to_png(code: &QrCode) -> Option<Vec<u8>> {
    let scale = 8u32;
    let quiet = 4u32;
    let qr_w = code.width() as u32;
    let img_size = (qr_w + quiet * 2) * scale;

    // Build raw RGBA pixel buffer
    let mut pixels: Vec<u8> = Vec::with_capacity((img_size * img_size * 4) as usize);
    for y in 0..img_size {
        for x in 0..img_size {
            let qx = (x / scale) as i32 - quiet as i32;
            let qy = (y / scale) as i32 - quiet as i32;
            let dark = if qx >= 0
                && qy >= 0
                && (qx as usize) < code.width()
                && (qy as usize) < code.width()
            {
                code[(qx as usize, qy as usize)] == qrcode::Color::Dark
            } else {
                false
            };
            let val = if dark { 0u8 } else { 255u8 };
            pixels.extend_from_slice(&[val, val, val, 255]);
        }
    }

    // Encode as PNG using minimal encoder (no extra deps — write raw PNG)
    Some(encode_png_rgba(&pixels, img_size, img_size))
}

/// Minimal PNG encoder for RGBA pixel data (no external dependency).
fn encode_png_rgba(pixels: &[u8], width: u32, height: u32) -> Vec<u8> {
    use std::io::Write;

    // Build raw image data with filter byte (0 = None) per row
    let row_len = (width as usize) * 4;
    let mut raw_data = Vec::with_capacity((row_len + 1) * height as usize);
    for row in 0..height as usize {
        raw_data.push(0u8); // filter: None
        raw_data.extend_from_slice(&pixels[row * row_len..(row + 1) * row_len]);
    }

    // Compress with flate2
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&raw_data).unwrap();
    let compressed = encoder.finish().unwrap();

    let mut png = Vec::new();

    // PNG signature
    png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);

    // IHDR chunk
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // color type: RGBA
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    write_png_chunk(&mut png, b"IHDR", &ihdr);

    // IDAT chunk
    write_png_chunk(&mut png, b"IDAT", &compressed);

    // IEND chunk
    write_png_chunk(&mut png, b"IEND", &[]);

    png
}

fn write_png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    let len = data.len() as u32;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(data);
    // CRC32 over chunk_type + data
    let crc = crc32(&[chunk_type.as_slice(), data].concat());
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

fn base64_encode_bytes(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Save QR code as a temporary PNG file. Returns the path on success.
fn save_qr_png(code: &QrCode) -> Option<std::path::PathBuf> {
    let png_data = qr_to_png(code)?;
    let path = std::env::temp_dir().join("rsclaw_qr.png");
    std::fs::write(&path, &png_data).ok()?;
    Some(path)
}

/// Save a URL as a QR-code PNG file silently (no terminal output, no image
/// viewer popup). Returns the path on success.
///
/// Used by headless callers (Tauri-spawned `rsclaw channels login --quiet`,
/// HTTP `/api/v1/channels/*/qr-login` endpoints) where the terminal-rendering
/// path of [`display_qr_terminal`] would write to a closed stdout or open an
/// unwanted preview window.
pub fn save_qr_to_path(url: &str) -> Result<std::path::PathBuf> {
    let code = QrCode::new(url.as_bytes()).context("failed to generate QR code")?;
    save_qr_png(&code).ok_or_else(|| anyhow::anyhow!("failed to write QR PNG to temp dir"))
}

/// Fallback: render QR code with Unicode half-block characters.
fn render_unicode(code: &QrCode) {
    let width = code.width();
    let quiet = 2usize;
    let total_w = width + quiet * 2;
    let total_h = width + quiet * 2;

    let is_dark = |x: i32, y: i32| -> bool {
        let qx = x - quiet as i32;
        let qy = y - quiet as i32;
        if qx < 0 || qy < 0 || qx >= width as i32 || qy >= width as i32 {
            return false;
        }
        code[(qx as usize, qy as usize)] == qrcode::Color::Dark
    };

    let mut y = 0i32;
    while y < total_h as i32 {
        let mut line = String::new();
        for x in 0..total_w as i32 {
            let top = is_dark(x, y);
            let bot = is_dark(x, y + 1);
            let ch = match (top, bot) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            };
            line.push(ch);
        }
        println!("  {line}");
        y += 2;
    }
}

/// ASCII art QR code — works on any terminal including dumb/serial consoles.
/// Uses "##" for dark modules and "  " for light, with inverted colors via
/// ANSI.
#[allow(dead_code)]
fn render_ascii(code: &QrCode) {
    let width = code.width();
    let quiet = 2usize;
    let total = width + quiet * 2;

    let is_dark = |x: i32, y: i32| -> bool {
        let qx = x - quiet as i32;
        let qy = y - quiet as i32;
        if qx < 0 || qy < 0 || qx >= width as i32 || qy >= width as i32 {
            return false;
        }
        code[(qx as usize, qy as usize)] == qrcode::Color::Dark
    };

    // Each module = 2 chars wide, 1 row high → roughly square in most monospace
    // fonts
    for y in 0..total as i32 {
        let mut line = String::with_capacity(total * 2 + 4);
        line.push_str("  ");
        for x in 0..total as i32 {
            if is_dark(x, y) {
                line.push_str("██");
            } else {
                line.push_str("  ");
            }
        }
        println!("{line}");
    }
}

/// Persist a login token to rsclaw.json5 config file.
///
/// Maps platform-specific keys to the canonical config field names:
///   - WeChat:   bot_token -> botToken, ilink_bot_id -> botId
///   - Feishu:   app_id -> appId, app_secret -> appSecret
///   - DingTalk: access_token -> accessToken, refresh_token -> refreshToken
pub fn save_token(platform: &str, token_data: &serde_json::Value) -> Result<()> {
    let config_path = crate::config::loader::detect_config_path()
        .ok_or_else(|| anyhow::anyhow!("no config file found"))?;

    let raw = std::fs::read_to_string(&config_path)?;
    let mut config: serde_json::Value = json5::from_str(&raw)?;

    // Ensure channels object exists
    let channels = config
        .as_object_mut()
        .context("config root is not an object")?
        .entry("channels")
        .or_insert_with(|| serde_json::json!({}));
    let channels = channels
        .as_object_mut()
        .context("channels is not an object")?;

    // Get or create the platform section
    let section = channels
        .entry(platform)
        .or_insert_with(|| serde_json::json!({}));
    let section = section
        .as_object_mut()
        .context("channel section is not an object")?;

    // Map platform-specific keys to config field names and merge
    match platform {
        "wechat" => {
            if let Some(v) = token_data.get("bot_token") {
                section.insert("botToken".to_owned(), v.clone());
            }
            if let Some(v) = token_data.get("ilink_bot_id") {
                section.insert("botId".to_owned(), v.clone());
            }
        }
        "feishu" => {
            if let Some(v) = token_data.get("app_id") {
                section.insert("appId".to_owned(), v.clone());
            }
            if let Some(v) = token_data.get("app_secret") {
                section.insert("appSecret".to_owned(), v.clone());
            }
            if let Some(v) = token_data.get("brand") {
                section.insert("brand".to_owned(), v.clone());
            }
        }
        "dingtalk" => {
            if let Some(v) = token_data.get("access_token") {
                section.insert("accessToken".to_owned(), v.clone());
            }
            if let Some(v) = token_data.get("refresh_token") {
                section.insert("refreshToken".to_owned(), v.clone());
            }
        }
        _ => {
            // Unknown platform: store all fields as-is
            if let Some(obj) = token_data.as_object() {
                for (k, v) in obj {
                    section.insert(k.clone(), v.clone());
                }
            }
        }
    }

    // Ensure the channel is enabled
    section.entry("enabled").or_insert(serde_json::Value::Bool(true));

    // Write back
    let json = serde_json::to_string_pretty(&config)?;
    std::fs::write(&config_path, &json)?;

    info!(platform, path = %config_path.display(), "auth token saved to config");
    Ok(())
}

/// Load a previously saved login token.
///
/// Reads from the config file's `channels.{platform}` section first.
/// Falls back to legacy `auth/{platform}.json` for backward compatibility.
pub fn load_token(platform: &str) -> Option<serde_json::Value> {
    // 1. Try reading from rsclaw.json5 config
    if let Some(config_path) = crate::config::loader::detect_config_path() {
        if let Ok(raw) = std::fs::read_to_string(&config_path) {
            if let Ok(config) = json5::from_str::<serde_json::Value>(&raw) {
                if let Some(section) = config.get("channels").and_then(|c| c.get(platform)) {
                    // Reverse-map config field names to legacy keys for callers
                    let mapped = reverse_map_token(platform, section);
                    if !mapped.as_object().is_some_and(|o| o.is_empty()) {
                        return Some(mapped);
                    }
                }
            }
        }
    }

    None
}

/// Reverse-map config field names back to the legacy token key names
/// so existing callers (startup.rs) continue to work.
fn reverse_map_token(platform: &str, section: &serde_json::Value) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    match platform {
        "wechat" => {
            if let Some(v) = section.get("botToken") {
                result.insert("bot_token".to_owned(), v.clone());
            }
            if let Some(v) = section.get("botId") {
                result.insert("ilink_bot_id".to_owned(), v.clone());
            }
        }
        "feishu" => {
            if let Some(v) = section.get("appId") {
                result.insert("app_id".to_owned(), v.clone());
            }
            if let Some(v) = section.get("appSecret") {
                result.insert("app_secret".to_owned(), v.clone());
            }
            if let Some(v) = section.get("brand") {
                result.insert("brand".to_owned(), v.clone());
            }
        }
        "dingtalk" => {
            if let Some(v) = section.get("accessToken") {
                result.insert("access_token".to_owned(), v.clone());
            }
            if let Some(v) = section.get("refreshToken") {
                result.insert("refresh_token".to_owned(), v.clone());
            }
        }
        _ => {
            if let Some(obj) = section.as_object() {
                return serde_json::Value::Object(obj.clone());
            }
        }
    }
    serde_json::Value::Object(result)
}
