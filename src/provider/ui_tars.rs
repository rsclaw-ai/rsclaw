//! UI-TARS VLM provider — HTTP client for remote GUI element detection.
//!
//! Calls a remote OpenAI-compatible API (e.g. vllm-mlx) that serves the
//! UI-TARS model.  The image is sent as a base64 data URI inside the
//! chat-completions request.

use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A detected UI element with normalized coordinates (0-1000).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiElement {
    pub element_type: String,
    pub label: String,
    /// Normalized coordinates [x, y] in 0-1000 range.
    pub coords: [u32; 2],
}

/// UI-TARS HTTP provider.
pub struct UiTarsProvider {
    client: reqwest::Client,
    api_url: String,
    api_key: Option<String>,
    model: String,
}

impl UiTarsProvider {
    /// Create a new provider.
    ///
    /// `api_url` — full URL to the chat completions endpoint
    ///             (e.g. `http://macstudio:8000/v1/chat/completions`).
    pub fn new(api_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: super::http_client(),
            api_url: api_url.into(),
            api_key,
            model: "ui-tars".to_owned(),
        }
    }

    /// Override the model name (default: "ui-tars").
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Analyze a screenshot and return detected UI elements.
    pub async fn analyze(&self, image_path: &str, max_tokens: u32) -> Result<Vec<UiElement>> {
        // Read image and encode to base64
        let image_bytes = tokio::fs::read(image_path)
            .await
            .with_context(|| format!("ui-tars: failed to read image {}", image_path))?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&image_bytes);

        // Determine MIME type from extension
        let mime = if image_path.ends_with(".png") {
            "image/png"
        } else if image_path.ends_with(".jpg") || image_path.ends_with(".jpeg") {
            "image/jpeg"
        } else {
            "image/png"
        };

        let data_uri = format!("data:{};base64,{}", mime, b64);

        let system_prompt = r#"You are a GUI automation assistant. Analyze the screenshot and list all interactive UI elements.
For each element, output exactly one line in this format:
type=<element_type>, label=<text>, coords=(<x>,<y>)
Coordinates are normalized 0-1000.
Only output the element lines, no extra text."#;

        let body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": data_uri}},
                    {"type": "text", "text": "List all interactive UI elements in this screenshot."}
                ]}
            ]
        });

        let mut req = self
            .client
            .post(&self.api_url)
            .json(&body)
            .header("Content-Type", "application/json");

        if let Some(key) = &self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }

        let resp = req
            .send()
            .await
            .context("ui-tars: HTTP request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("ui-tars: API returned {}: {}", status, text);
        }

        let resp_json: Value = resp
            .json()
            .await
            .context("ui-tars: failed to parse JSON response")?;

        let content = resp_json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("");

        let elements = parse_ui_tars_response(content);
        Ok(elements)
    }

    /// Scale normalized coords (0-1000) to actual screen pixels.
    pub fn scale_coords(elements: &[UiElement], screen_w: u32, screen_h: u32) -> Vec<UiElement> {
        elements
            .iter()
            .map(|e| UiElement {
                element_type: e.element_type.clone(),
                label: e.label.clone(),
                coords: [
                    (e.coords[0] as f64 / 1000.0 * screen_w as f64) as u32,
                    (e.coords[1] as f64 / 1000.0 * screen_h as f64) as u32,
                ],
            })
            .collect()
    }
}

/// Parse UI-TARS text response into structured elements.
fn parse_ui_tars_response(text: &str) -> Vec<UiElement> {
    let mut elements = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let element_type = extract_field(line, "type=").unwrap_or("unknown").to_owned();
        let label = extract_field(line, "label=").unwrap_or("").to_owned();
        let coords = extract_field(line, "coords=")
            .and_then(|v| {
                let v = v.trim_start_matches('=').trim();
                v.strip_prefix('(').and_then(|s| s.strip_suffix(')')).and_then(|inner| {
                    let mut nums = inner.split(',');
                    let x = nums.next()?.trim().parse().ok()?;
                    let y = nums.next()?.trim().parse().ok()?;
                    Some([x, y])
                })
            })
            .unwrap_or([0, 0]);

        if element_type != "unknown" || coords != [0, 0] {
            elements.push(UiElement {
                element_type,
                label,
                coords,
            });
        }
    }
    elements
}

/// Extract a `prefix=value` field from a comma-separated line.
/// Handles commas inside parentheses (e.g. `coords=(500,750)`).
fn extract_field<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let start = line.find(prefix)? + prefix.len();
    let rest = &line[start..];
    let mut depth = 0i32;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => return Some(rest[..i].trim()),
            _ => {}
        }
    }
    Some(rest.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_coords_roundtrip() {
        let elements = vec![UiElement {
            element_type: "button".to_owned(),
            label: "Send".to_owned(),
            coords: [500, 750],
        }];
        let scaled = UiTarsProvider::scale_coords(&elements, 1920, 1080);
        assert_eq!(scaled[0].coords, [960, 810]);
    }

    #[test]
    fn parse_response() {
        let text = "type=button, label=Send, coords=(500,750)\ntype=input, label=Search, coords=(200,300)";
        let elems = parse_ui_tars_response(text);
        assert_eq!(elems.len(), 2);
        assert_eq!(elems[0].element_type, "button");
        assert_eq!(elems[0].label, "Send");
        assert_eq!(elems[0].coords, [500, 750]);
        assert_eq!(elems[1].element_type, "input");
        assert_eq!(elems[1].coords, [200, 300]);
    }
}
