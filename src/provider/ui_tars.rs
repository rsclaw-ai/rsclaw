//! UI-TARS VLM provider — HTTP client for remote GUI automation.
//!
//! Calls a remote OpenAI-compatible API (e.g. mlx_vlm server) that serves the
//! UI-TARS model.  The image is sent as a base64 data URI inside the
//! chat-completions request.
//!
//! UI-TARS native output format (step-by-step):
//!   Thought: I need to click the send button.
//!   Action: click(start_box='<bbox>x1 y1 x2 y2</bbox>')
//!
//! We parse both the native Thought/Action format and a simplified
//! type/label/coords format so the agent can consume the result flexibly.

use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A detected UI element or automation step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiElement {
    pub element_type: String,
    pub label: String,
    /// Normalized coordinates [x, y] in 0-1000 range.
    pub coords: [u32; 2],
    /// UI-TARS thought (reasoning) for this step.
    pub thought: Option<String>,
    /// Raw UI-TARS action string.
    pub action: Option<String>,
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

    /// Analyze a screenshot and return detected UI elements / steps.
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

        // Use the default system prompt ("You are a helpful assistant.")
        // UI-TARS is trained with this default; custom system prompts cause
        // the model to regress into training-data patterns.
        let body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "messages": [
                {"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": data_uri}},
                    {"type": "text", "text": "Task: Observe the screen and identify all interactive UI elements. For each element output:\nThought: <your reasoning>\nAction: <action>(start_box='<bbox>x1 y1 x2 y2</bbox>')\nSupported actions: click, type, scroll, hotkey. Coordinates are in 0-1000 range."}
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
                thought: e.thought.clone(),
                action: e.action.clone(),
            })
            .collect()
    }
}

/// Parse UI-TARS text response into structured elements.
///
/// Supports both:
///   Thought: ...\nAction: click(start_box='<bbox>x1 y1 x2 y2</bbox>')\n
///   type=button, label=Send, coords=(500,750)
fn parse_ui_tars_response(text: &str) -> Vec<UiElement> {
    let mut elements = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();
        if line.is_empty() {
            i += 1;
            continue;
        }

        // Try native Thought/Action format first
        if line.starts_with("Thought:") {
            let thought = line.strip_prefix("Thought:").unwrap_or("").trim().to_owned();
            let mut action = String::new();
            let mut action_type = String::new();
            let mut coords = [0u32, 0];
            let mut label = String::new();

            i += 1;
            if i < lines.len() {
                let action_line = lines[i].trim();
                if action_line.starts_with("Action:") {
                    action = action_line.strip_prefix("Action:").unwrap_or("").trim().to_owned();
                    // Parse action type and bbox
                    (action_type, label, coords) = parse_action(&action);
                    i += 1;
                }
            }

            elements.push(UiElement {
                element_type: action_type,
                label,
                coords,
                thought: Some(thought),
                action: Some(action),
            });
            continue;
        }

        // Fallback to simple type/label/coords format
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
                thought: None,
                action: None,
            });
        }
        i += 1;
    }
    elements
}

/// Parse a UI-TARS action string into (action_type, label, center_coords).
///
/// Examples:
///   click(start_box='<bbox>450 780 500 820</bbox>')
///   type(content='Hello world')
///   scroll(start_box='<bbox>100 200 300 400</bbox>', direction='down')
fn parse_action(action: &str) -> (String, String, [u32; 2]) {
    let action = action.trim();

    // Extract action type: everything before first '('
    let action_type = action.split('(').next().unwrap_or("unknown").trim().to_lowercase();

    // Extract bbox from <bbox>...</bbox>
    let coords = if let Some(start) = action.find("<bbox>") {
        if let Some(end) = action.find("</bbox>") {
            let inner = &action[start + 6..end];
            let nums: Vec<&str> = inner.split_whitespace().collect();
            if nums.len() == 4 {
                if let (Ok(x1), Ok(y1), Ok(x2), Ok(y2)) = (
                    nums[0].parse::<f64>(),
                    nums[1].parse::<f64>(),
                    nums[2].parse::<f64>(),
                    nums[3].parse::<f64>(),
                ) {
                    let cx = ((x1 + x2) / 2.0).clamp(0.0, 1000.0) as u32;
                    let cy = ((y1 + y2) / 2.0).clamp(0.0, 1000.0) as u32;
                    [cx, cy]
                } else {
                    [0, 0]
                }
            } else {
                [0, 0]
            }
        } else {
            [0, 0]
        }
    } else {
        [0, 0]
    };

    // Extract label from content='...' or description
    let label = if let Some(start) = action.find("content='") {
        let rest = &action[start + 9..];
        rest.split('\'').next().unwrap_or("").to_owned()
    } else {
        String::new()
    };

    (action_type, label, coords)
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
            thought: None,
            action: None,
        }];
        let scaled = UiTarsProvider::scale_coords(&elements, 1920, 1080);
        assert_eq!(scaled[0].coords, [960, 810]);
    }

    #[test]
    fn parse_simple_response() {
        let text = "type=button, label=Send, coords=(500,750)\ntype=input, label=Search, coords=(200,300)";
        let elems = parse_ui_tars_response(text);
        assert_eq!(elems.len(), 2);
        assert_eq!(elems[0].element_type, "button");
        assert_eq!(elems[0].label, "Send");
        assert_eq!(elems[0].coords, [500, 750]);
        assert_eq!(elems[1].element_type, "input");
        assert_eq!(elems[1].coords, [200, 300]);
    }

    #[test]
    fn parse_thought_action_click() {
        let text = "Thought: I need to click the send button.\nAction: click(start_box='<bbox>450 780 500 820</bbox>')";
        let elems = parse_ui_tars_response(text);
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].element_type, "click");
        assert_eq!(elems[0].thought.as_deref(), Some("I need to click the send button."));
        assert_eq!(elems[0].action.as_deref(), Some("click(start_box='<bbox>450 780 500 820</bbox>')"));
        // Center of (450,780)-(500,820) = (475,800)
        assert_eq!(elems[0].coords, [475, 800]);
    }

    #[test]
    fn parse_thought_action_type() {
        let text = "Thought: Type the search query.\nAction: type(content='hello')";
        let elems = parse_ui_tars_response(text);
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].element_type, "type");
        assert_eq!(elems[0].label, "hello");
        assert_eq!(elems[0].thought.as_deref(), Some("Type the search query."));
    }
}
