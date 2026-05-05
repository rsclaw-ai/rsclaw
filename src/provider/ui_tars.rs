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
use std::collections::HashMap;

/// A detected UI element or automation step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiElement {
    pub element_type: String,
    pub label: String,
    /// Coordinates [x, y].
    /// For V1.0: normalized 0-1000 range.
    /// For V1.5: pixel coordinates in the smart-resized image space.
    pub coords: [u32; 2],
    /// Smart-resized image dimensions used by the model (V1.5 only).
    pub smart_resize_dims: Option<[u32; 2]>,
    /// UI-TARS thought (reasoning) for this step.
    pub thought: Option<String>,
    /// Raw UI-TARS action string.
    pub action: Option<String>,
}

/// A parsed UI-TARS action for agent loop execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiTarsAction {
    pub action_type: String,
    pub thought: String,
    pub action_inputs: HashMap<String, String>,
}

/// UI-TARS model version.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UiTarsVersion {
    V1_0,
    V1_5,
}

/// UI-TARS HTTP provider.
pub struct UiTarsProvider {
    client: reqwest::Client,
    api_url: String,
    api_key: Option<String>,
    model: String,
    version: UiTarsVersion,
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
            version: UiTarsVersion::V1_5,
        }
    }

    /// Override the model name (default: "ui-tars").
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Set the model version (default: V1_5).
    pub fn with_version(mut self, version: UiTarsVersion) -> Self {
        self.version = version;
        self
    }

    /// Analyze a screenshot and return detected UI elements / steps.
    pub async fn analyze(&self, image_path: &str, max_tokens: u32) -> Result<Vec<UiElement>> {
        // Read image and encode to base64
        let image_bytes = tokio::fs::read(image_path)
            .await
            .with_context(|| format!("ui-tars: failed to read image {}", image_path))?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&image_bytes);

        // Get image dimensions for smart-resize calculation
        let (img_w, img_h) = image_dimensions(&image_bytes)
            .unwrap_or((1920, 1080));

        // Calculate smart-resize dimensions for V1.5
        let smart_dims = if self.version == UiTarsVersion::V1_5 {
            smart_resize_v15(img_h, img_w)
        } else {
            None
        };

        // Determine MIME type from extension
        let mime = if image_path.ends_with(".png") {
            "image/png"
        } else if image_path.ends_with(".jpg") || image_path.ends_with(".jpeg") {
            "image/jpeg"
        } else {
            "image/png"
        };

        let data_uri = format!("data:{};base64,{},", mime, b64);

        let max_tokens = if self.version == UiTarsVersion::V1_5 {
            2048
        } else {
            max_tokens
        };

        // Build messages matching the UI-TARS training data format (same as predict).
        // UI-TARS 1.5 is trained on single-step action prediction, not open-ended
        // element enumeration. Use a concrete task prompt to get usable output.
        let system_prompt = r#"You are a GUI agent. Given a screenshot, identify interactive UI elements.

## Output Format
```
Thought: ...
Action: ...
```

## Action Space
click(start_box='<|box_start|>(x1, y1)<|box_end|>')

## Note
- Use Chinese in Thought part.
- Output one Thought/Action pair per element.
"#;
        let instruction = "Find all interactive UI elements (buttons, inputs, links, icons) on the screen. For each one, output its location using a click action with coordinates.";

        let mut messages = vec![];
        messages.push(json!({
            "role": "user",
            "content": format!("{}\n\n## User Instruction\n{}", system_prompt, instruction)
        }));
        messages.push(json!({
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": data_uri}}
            ]
        }));

        let body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "temperature": 0,
            "messages": messages
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

        tracing::info!(
            content = %content,
            img_w = img_w,
            img_h = img_h,
            "ui-tars analyze response"
        );

        let elements = parse_ui_tars_response(content, smart_dims);
        Ok(elements)
    }

    /// Scale coords to actual screen pixels.
    ///
    /// For V1.0: coords are normalized 0-1000.
    /// For V1.5: coords are in smart-resized image space; scale by
    /// screen_size / smart_resize_size.
    pub fn scale_coords(elements: &[UiElement], screen_w: u32, screen_h: u32) -> Vec<UiElement> {
        elements
            .iter()
            .map(|e| {
                let (sx, sy) = if let Some([sr_w, sr_h]) = e.smart_resize_dims {
                    // V1.5: coords are in smart-resized image space
                    (screen_w as f64 / sr_w as f64, screen_h as f64 / sr_h as f64)
                } else {
                    // V1.0: coords are normalized 0-1000
                    (screen_w as f64 / 1000.0, screen_h as f64 / 1000.0)
                };
                UiElement {
                    element_type: e.element_type.clone(),
                    label: e.label.clone(),
                    coords: [
                        (e.coords[0] as f64 * sx).clamp(0.0, screen_w as f64) as u32,
                        (e.coords[1] as f64 * sy).clamp(0.0, screen_h as f64) as u32,
                    ],
                    smart_resize_dims: e.smart_resize_dims,
                    thought: e.thought.clone(),
                    action: e.action.clone(),
                }
            })
            .collect()
    }

    /// Predict the next action(s) for a GUI task using UI-TARS model.
    ///
    /// Sends the screenshot + instruction to the model and returns parsed actions.
    ///
    /// `img_w`/`img_h` — dimensions of the screenshot image the model receives.
    /// `screen_w`/`screen_h` — actual screen resolution in physical pixels.
    ///   When the screenshot was resized before sending, coords are first mapped
    ///   from smart-resized space → image space → screen space.
    pub async fn predict(
        &self,
        system_prompt: &str,
        instruction: &str,
        screenshot_base64: &str,
        img_w: u32,
        img_h: u32,
        screen_w: u32,
        screen_h: u32,
        history: &[(String, String)], // (thought, action) pairs
    ) -> Result<Vec<UiTarsAction>> {
        // Build messages matching the UI-TARS training data format:
        //   user: system prompt (plain text)
        //   assistant: Thought/Action (history)
        //   user: screenshot (image only)
        let mut messages = vec![];

        // First turn: system prompt + instruction as plain text user message
        let text_content = format!("{}\n\n## User Instruction\n{}", system_prompt, instruction);
        messages.push(json!({
            "role": "user",
            "content": text_content
        }));

        // History turns: assistant Thought/Action pairs
        for (thought, action) in history {
            messages.push(json!({
                "role": "assistant",
                "content": format!("Thought: {}\nAction: {}", thought, action)
            }));
        }

        // Current turn: screenshot only (no text)
        messages.push(json!({
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", screenshot_base64)}}
            ]
        }));

        let max_tokens = if self.version == UiTarsVersion::V1_5 {
            2048
        } else {
            1000
        };

        let body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "temperature": 0,
            "messages": messages
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

        tracing::info!(
            content = %content,
            img_w = img_w,
            img_h = img_h,
            screen_w = screen_w,
            screen_h = screen_h,
            "ui-tars predict response"
        );

        let smart_dims = smart_resize_v15(img_h, img_w);
        let actions = parse_ui_tars_prediction(content, smart_dims, img_w, img_h, screen_w, screen_h);
        Ok(actions)
    }
}

/// Parse UI-TARS text response into structured elements.
///
/// Supports both:
///   Thought: ...\nAction: click(start_box='<bbox>x1 y1 x2 y2</bbox>')\n
///   type=button, label=Send, coords=(500,750)
fn parse_ui_tars_response(text: &str, smart_dims: Option<[u32; 2]>) -> Vec<UiElement> {
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
                smart_resize_dims: smart_dims,
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
                smart_resize_dims: smart_dims,
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
                    let cx = ((x1 + x2) / 2.0).max(0.0) as u32;
                    let cy = ((y1 + y2) / 2.0).max(0.0) as u32;
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

/// Parse a UI-TARS prediction response into structured actions.
///
/// Supports the format:
///   Thought: ...
///   Action: click(start_box='[x1, y1, x2, y2]')
///
/// Coordinates are mapped: smart-resized space → image space → screen space.
fn parse_ui_tars_prediction(
    text: &str,
    smart_dims: Option<[u32; 2]>,
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
) -> Vec<UiTarsAction> {
    let mut actions = Vec::new();

    // Extract thought
    let thought = if let Some(start) = text.find("Thought:") {
        let rest = &text[start + 8..];
        if let Some(end) = rest.find("\nAction:") {
            rest[..end].trim().to_owned()
        } else {
            rest.trim().to_owned()
        }
    } else {
        String::new()
    };

    // Extract action string
    let action_str = if let Some(start) = text.find("Action:") {
        let rest = &text[start + 7..];
        rest.trim().lines().next().unwrap_or("").trim().to_owned()
    } else {
        text.trim().lines().next().unwrap_or("").trim().to_owned()
    };

    if action_str.is_empty() {
        return actions;
    }

    let mut inputs = HashMap::new();

    // Parse action type
    let action_type = action_str.split('(').next().unwrap_or("unknown").trim().to_lowercase();

    // Extract arguments from inside parentheses
    if let Some(start) = action_str.find('(') {
        if let Some(end) = action_str.rfind(')') {
            let args_str = &action_str[start + 1..end];
            let arg_pairs = parse_arg_pairs(args_str);
            for (key, value) in arg_pairs {
                // Parse bbox/point coordinates
                if key.contains("box") {
                    if let Some(coords) = parse_box_value(&value) {
                        let (cx, cy) = if let Some([sr_w, sr_h]) = smart_dims {
                            // V1.5: coords are in smart-resized space.
                            // Map to logical image space (same space as the image
                            // sent to the model).  exec_args multiplies by scale
                            // so xy() (which divides by scale) ends up with the
                            // correct logical coordinates for cliclick.
                            let logical_cx = (coords.0 * img_w as f64 / sr_w as f64)
                                .clamp(0.0, img_w as f64) as u32;
                            let logical_cy = (coords.1 * img_h as f64 / sr_h as f64)
                                .clamp(0.0, img_h as f64) as u32;
                            (logical_cx, logical_cy)
                        } else {
                            // V1.0: coords are 0-1000 normalized
                            let cx = (coords.0 / 1000.0 * img_w as f64) as u32;
                            let cy = (coords.1 / 1000.0 * img_h as f64) as u32;
                            (cx, cy)
                        };
                        inputs.insert(format!("{}_x", key), cx.to_string());
                        inputs.insert(format!("{}_y", key), cy.to_string());
                        inputs.insert(key, format!("{},{},{},{}", coords.0, coords.1, coords.2, coords.3));
                    }
                } else if key == "x" || key == "y" {
                    // UI-TARS sometimes outputs click(x=351, y=208) directly.
                    // Store under start_box_{x|y} so the exec loop can pick them up.
                    if let Ok(v) = value.parse::<f64>() {
                        let scaled = if let Some([sr_w, sr_h]) = smart_dims {
                            let dim = if key == "x" { img_w } else { img_h };
                            (v * dim as f64 / sr_w as f64).clamp(0.0, dim as f64) as u32
                        } else {
                            let dim = if key == "x" { img_w } else { img_h };
                            (v / 1000.0 * dim as f64) as u32
                        };
                        inputs.insert(format!("start_box_{key}"), scaled.to_string());
                    }
                } else {
                    inputs.insert(key, value);
                }
            }
        }
    }

    actions.push(UiTarsAction {
        action_type,
        thought,
        action_inputs: inputs,
    });

    actions
}

/// Parse argument pairs from action string, e.g.:
///   start_box='[x1, y1, x2, y2]', direction='down'
fn parse_arg_pairs(args_str: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut in_quote = false;
    let mut quote_char = '\0';
    let mut seen_eq = false;

    for (i, c) in args_str.chars().enumerate() {
        match c {
            '(' | '[' | '{' => {
                depth += 1;
                current.push(c);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(c);
            }
            '\'' | '"' => {
                if !in_quote {
                    in_quote = true;
                    quote_char = c;
                    current.push(c);
                } else if c == quote_char {
                    in_quote = false;
                    current.push(c);
                } else {
                    current.push(c);
                }
            }
            '=' if depth == 0 && !in_quote => {
                seen_eq = true;
                current.push(c);
            }
            ',' if depth == 0 && !in_quote && seen_eq => {
                // We have already seen '=' so this comma may be inside an
                // unquoted value (e.g. start_box=1164,866,1164,866).
                // Peek ahead: only treat as separator if what follows looks
                // like a new key=value pair (key is alphanumeric + underscore).
                let rest = &args_str[i + 1..];
                let trimmed = rest.trim_start();
                let looks_like_new_key = trimmed
                    .split(|c: char| c == '=')
                    .next()
                    .map(|s| {
                        let s = s.trim_end();
                        !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
                    })
                    .unwrap_or(false);
                if looks_like_new_key {
                    if let Some(pair) = parse_single_pair(&current) {
                        pairs.push(pair);
                    }
                    current.clear();
                    seen_eq = false;
                } else {
                    current.push(c);
                }
            }
            ',' if depth == 0 && !in_quote => {
                if let Some(pair) = parse_single_pair(&current) {
                    pairs.push(pair);
                }
                current.clear();
                seen_eq = false;
            }
            _ => current.push(c),
        }
    }

    if !current.is_empty() {
        if let Some(pair) = parse_single_pair(&current) {
            pairs.push(pair);
        }
    }

    pairs
}

fn parse_single_pair(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    let eq_pos = s.find('=')?;
    let key = s[..eq_pos].trim().to_owned();
    let value = s[eq_pos + 1..].trim().to_owned();
    // Remove surrounding quotes
    let value = value
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| value.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(&value)
        .to_owned();
    Some((key, value))
}

/// Parse a box/point value string into (x1, y1, x2, y2).
/// Supports formats:
///   [0.123,0.456,0.789,0.999]
///   (100,200)
///   <bbox>450 780 500 820</bbox>
fn parse_box_value(value: &str) -> Option<(f64, f64, f64, f64)> {
    let trimmed = value.trim();

    // Format: bare comma-separated numbers (e.g. "1164,866,1164,866")
    // Must come before () check because bare numbers have no brackets.
    if !trimmed.starts_with('[')
        && !trimmed.starts_with('(')
        && !trimmed.starts_with('<')
        && trimmed.contains(',')
    {
        let nums: Vec<&str> = trimmed.split(',').map(|s| s.trim()).collect();
        if nums.len() == 4 {
            let x1 = nums[0].parse::<f64>().ok()?;
            let y1 = nums[1].parse::<f64>().ok()?;
            let x2 = nums[2].parse::<f64>().ok()?;
            let y2 = nums[3].parse::<f64>().ok()?;
            return Some((x1, y1, x2, y2));
        }
        if nums.len() == 2 {
            let x = nums[0].parse::<f64>().ok()?;
            let y = nums[1].parse::<f64>().ok()?;
            return Some((x, y, x, y));
        }
    }

    // Format: [x1,y1,x2,y2]
    if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let nums: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
        if nums.len() == 4 {
            let x1 = nums[0].parse::<f64>().ok()?;
            let y1 = nums[1].parse::<f64>().ok()?;
            let x2 = nums[2].parse::<f64>().ok()?;
            let y2 = nums[3].parse::<f64>().ok()?;
            return Some((x1, y1, x2, y2));
        }
        // Point format: [x,y]
        if nums.len() == 2 {
            let x = nums[0].parse::<f64>().ok()?;
            let y = nums[1].parse::<f64>().ok()?;
            return Some((x, y, x, y));
        }
    }

    // Format: (x1,y1) or (x1,y1,x2,y2)
    if let Some(inner) = trimmed.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        let nums: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
        if nums.len() == 2 {
            let x = nums[0].parse::<f64>().ok()?;
            let y = nums[1].parse::<f64>().ok()?;
            return Some((x, y, x, y));
        }
        if nums.len() == 4 {
            let x1 = nums[0].parse::<f64>().ok()?;
            let y1 = nums[1].parse::<f64>().ok()?;
            let x2 = nums[2].parse::<f64>().ok()?;
            let y2 = nums[3].parse::<f64>().ok()?;
            return Some((x1, y1, x2, y2));
        }
    }

    // Format: <bbox>x1 y1 x2 y2</bbox>
    if let Some(start) = trimmed.find("<bbox>") {
        if let Some(end) = trimmed.find("</bbox>") {
            let inner = &trimmed[start + 6..end];
            let nums: Vec<&str> = inner.split_whitespace().collect();
            if nums.len() == 4 {
                let x1 = nums[0].parse::<f64>().ok()?;
                let y1 = nums[1].parse::<f64>().ok()?;
                let x2 = nums[2].parse::<f64>().ok()?;
                let y2 = nums[3].parse::<f64>().ok()?;
                return Some((x1, y1, x2, y2));
            }
        }
    }

    // Format: <|box_start|>(x1,y1)<|box_end|>
    if let Some(start) = trimmed.find("<|box_start|>") {
        if let Some(end) = trimmed.find("<|box_end|>") {
            let inner = &trimmed[start + 13..end];
            let inner = inner.trim();
            // Strip surrounding parentheses if present
            let inner = inner
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .unwrap_or(inner);
            let nums: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
            if nums.len() == 2 {
                let x = nums[0].parse::<f64>().ok()?;
                let y = nums[1].parse::<f64>().ok()?;
                return Some((x, y, x, y));
            }
        }
    }

    None
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

/// Get image dimensions from raw bytes (PNG/JPEG).
pub fn image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    use image::ImageReader;
    use std::io::Cursor;
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    Some((w, h))
}

/// Smart-resize dimensions for UI-TARS V1.5.
///
/// Matches the Python implementation:
/// - Round dimensions to factor=28
/// - Ensure total pixels within [minPixels, maxPixels]
/// - Returns (width, height) after smart resize
fn smart_resize_v15(height: u32, width: u32) -> Option<[u32; 2]> {
    const FACTOR: u32 = 28;
    const MIN_PIXELS: u32 = 100 * FACTOR * FACTOR; // 78_400
    const MAX_PIXELS: u32 = 16384 * FACTOR * FACTOR; // 12_845_056
    const MAX_RATIO: f64 = 200.0;

    let h = height as f64;
    let w = width as f64;

    if h.max(w) / h.min(w) > MAX_RATIO {
        return None;
    }

    let round_by = |n: f64, f: u32| (n / f as f64).round() * f as f64;
    let floor_by = |n: f64, f: u32| (n / f as f64).floor() * f as f64;
    let ceil_by = |n: f64, f: u32| (n / f as f64).ceil() * f as f64;

    let mut w_bar = (FACTOR as f64).max(round_by(w, FACTOR));
    let mut h_bar = (FACTOR as f64).max(round_by(h, FACTOR));

    let pixels = h_bar * w_bar;
    if pixels > MAX_PIXELS as f64 {
        let beta = ((h * w) / MAX_PIXELS as f64).sqrt();
        h_bar = floor_by(h / beta, FACTOR);
        w_bar = floor_by(w / beta, FACTOR);
    } else if pixels < MIN_PIXELS as f64 {
        let beta = (MIN_PIXELS as f64 / (h * w)).sqrt();
        h_bar = ceil_by(h * beta, FACTOR);
        w_bar = ceil_by(w * beta, FACTOR);
    }

    Some([w_bar as u32, h_bar as u32])
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
            smart_resize_dims: None,
            thought: None,
            action: None,
        }];
        let scaled = UiTarsProvider::scale_coords(&elements, 1920, 1080);
        assert_eq!(scaled[0].coords, [960, 810]);
    }

    #[test]
    fn scale_coords_v15_smart_resize() {
        // Model outputs coords in 1344x756 smart-resized space
        let elements = vec![UiElement {
            element_type: "button".to_owned(),
            label: "Send".to_owned(),
            coords: [672, 378], // center of 1344x756
            smart_resize_dims: Some([1344, 756]),
            thought: None,
            action: None,
        }];
        let scaled = UiTarsProvider::scale_coords(&elements, 1920, 1080);
        // 672 * (1920/1344) = 960, 378 * (1080/756) = 540
        assert_eq!(scaled[0].coords, [960, 540]);
    }

    #[test]
    fn parse_simple_response() {
        let text = "type=button, label=Send, coords=(500,750)\ntype=input, label=Search, coords=(200,300)";
        let elems = parse_ui_tars_response(text, None);
        assert_eq!(elems.len(), 2);
        assert_eq!(elems[0].element_type, "button");
        assert_eq!(elems[0].label, "Send");
        assert_eq!(elems[0].coords, [500, 750]);
        assert_eq!(elems[1].element_type, "input");
        assert_eq!(elems[1].label, "Search");
        assert_eq!(elems[1].coords, [200, 300]);
    }

    #[test]
    fn parse_thought_action_click() {
        let text = "Thought: I need to click the send button.\nAction: click(start_box='<bbox>450 780 500 820</bbox>')";
        let elems = parse_ui_tars_response(text, None);
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
        let elems = parse_ui_tars_response(text, None);
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].element_type, "type");
        assert_eq!(elems[0].label, "hello");
        assert_eq!(elems[0].thought.as_deref(), Some("Type the search query."));
    }

    #[test]
    fn parse_box_start_end_format() {
        // UI-TARS 1.5 uses <|box_start|>(x,y)<|box_end|> format
        let text = "Thought: Click the button.\nAction: click(start_box='<|box_start|>(133,863)<|box_end|>')";
        let smart_dims = smart_resize_v15(900, 1440);
        let actions = parse_ui_tars_prediction(text, smart_dims, 1440, 900, 1440, 900);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type, "click");
        // With smart_resize, 133,863 in smart-resized space maps to screen space
        assert!(actions[0].action_inputs.get("start_box_x").is_some());
        assert!(actions[0].action_inputs.get("start_box_y").is_some());
    }

    #[test]
    fn parse_unquoted_box_with_extra_coords() {
        // UI-TARS sometimes outputs malformed box values like:
        // start_box=1209,838,1209,838, start_box_y=841, start_box_x=1219
        // The first start_box must capture the full comma-separated value.
        let text = "Thought: Click icon.\nAction: click(start_box=1209,838,1209,838, start_box_y=841, start_box_x=1219)";
        let actions = parse_ui_tars_prediction(text, None, 1440, 900, 1440, 900);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type, "click");
        // start_box should parse the full 4-number value
        assert_eq!(
            actions[0].action_inputs.get("start_box"),
            Some(&"1209,838,1209,838".to_string())
        );
        // x/y should be extracted from start_box (not from the extra keys)
        let x = actions[0].action_inputs.get("start_box_x").unwrap().parse::<u32>().unwrap();
        let y = actions[0].action_inputs.get("start_box_y").unwrap().parse::<u32>().unwrap();
        assert!(x > 0, "start_box_x should be > 0, got {}", x);
        assert!(y > 0, "start_box_y should be > 0, got {}", y);
    }

    #[test]
    fn smart_resize_v15_1920x1080() {
        let dims = smart_resize_v15(1080, 1920).unwrap();
        // 1920x1080 already within maxPixels; rounded to factor 28
        assert_eq!(dims[0] % 28, 0);
        assert_eq!(dims[1] % 28, 0);
        assert!(dims[0] >= 1920 - 28);
        assert!(dims[1] >= 1080 - 28);
    }

    #[test]
    fn smart_resize_v15_oversized() {
        // A very large image should be scaled down
        let dims = smart_resize_v15(8000, 8000).unwrap();
        assert_eq!(dims[0] % 28, 0);
        assert_eq!(dims[1] % 28, 0);
        assert!(dims[0] * dims[1] <= 12_845_056);
    }
}
