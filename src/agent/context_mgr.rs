//! Context management — pruning, budget trimming, compaction helpers.
//!
//! Extracted from `runtime.rs` to reduce file size.

use crate::{
    config::schema::ContextPruningConfig,
    provider::{ContentPart, Message, MessageContent, Role, ToolDef},
};

/// Estimate token count for mixed-language text.
/// - ASCII/Latin: ~4 chars per token
/// - CJK (Chinese/Japanese/Korean): ~1.5 chars per token
/// - Other Unicode: ~2 chars per token
pub fn estimate_tokens(text: &str) -> usize {
    let mut ascii_chars = 0usize;
    let mut cjk_chars = 0usize;
    let mut other_chars = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_chars += 1;
        } else if ('\u{4E00}'..='\u{9FFF}').contains(&ch)
            || ('\u{3400}'..='\u{4DBF}').contains(&ch)
            || ('\u{3000}'..='\u{303F}').contains(&ch)
            || ('\u{FF00}'..='\u{FFEF}').contains(&ch)
            || ('\u{AC00}'..='\u{D7AF}').contains(&ch)
        {
            cjk_chars += 1;
        } else {
            other_chars += 1;
        }
    }
    ascii_chars / 4 + (cjk_chars * 2 + 1) / 3 + other_chars / 2 + 1
}

/// Strip image data URIs from all but the last user message to prevent
/// context bloat.
pub(crate) fn strip_old_images(mut messages: Vec<Message>) -> Vec<Message> {
    // Find the index of the last user message (the one that may have fresh images).
    let last_user_idx = messages.iter().rposition(|m| m.role == Role::User);

    for (i, msg) in messages.iter_mut().enumerate() {
        if Some(i) == last_user_idx {
            continue; // keep images on the latest user message
        }
        if let MessageContent::Parts(parts) = &msg.content {
            let has_image = parts.iter().any(|p| matches!(p, ContentPart::Image { .. }));
            if has_image {
                // Replace with text-only version
                let text: String = parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                msg.content = MessageContent::Text(if text.is_empty() {
                    "[image]".to_owned()
                } else {
                    format!("{text} [image]")
                });
            }
        }
    }
    messages
}

/// Prune the session message history in-place according to config.
///
/// Strategy (applied in order):
///   1. Hard-clear: if total chars > threshold, keep only the last user message.
///   2. Soft-trim: if total chars > tail_chars limit, remove old Tool messages.
pub(crate) fn apply_context_pruning(messages: &mut Vec<Message>, cfg: Option<&ContextPruningConfig>) {
    let Some(cfg) = cfg else { return };

    let total: usize = messages.iter().map(msg_chars).sum();

    // Hard clear.
    if let Some(hc) = &cfg.hard_clear
        && hc.enabled.unwrap_or(false)
    {
        let threshold = hc.threshold.unwrap_or(200_000) as usize;
        if total > threshold {
            let last_user = messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .cloned();
            messages.clear();
            if let Some(m) = last_user {
                messages.push(m);
            }
            return;
        }
    }

    // Soft trim.
    if let Some(st) = &cfg.soft_trim
        && st.enabled.unwrap_or(false)
    {
        let limit = st.tail_chars.unwrap_or(80_000) as usize;
        let min_prunable = cfg.min_prunable_tool_chars.unwrap_or(500) as usize;

        if total > limit {
            let mut chars_over = total - limit;
            let mut to_remove: Vec<usize> = Vec::new();
            for (i, msg) in messages.iter().enumerate() {
                if chars_over == 0 {
                    break;
                }
                if msg.role == Role::Tool {
                    let c = msg_chars(msg);
                    if c >= min_prunable {
                        to_remove.push(i);
                        chars_over = chars_over.saturating_sub(c);
                    }
                }
            }
            for i in to_remove.into_iter().rev() {
                messages.remove(i);
            }
        }
    }
}

/// Count characters in a message (used by pruning).
pub(crate) fn msg_chars(m: &Message) -> usize {
    match &m.content {
        MessageContent::Text(t) => t.len(),
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => text.len(),
                _ => 50,
            })
            .sum(),
    }
}

/// Build a summary Message from the last 10 user/assistant messages (for /clear).
pub(crate) fn build_clear_summary(messages: &[Message]) -> Option<Message> {
    if messages.is_empty() { return None; }
    let recent: Vec<&Message> = messages.iter().rev().take(10).rev().collect();
    let mut parts = Vec::new();
    for m in &recent {
        let role = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            _ => continue,
        };
        let text = match &m.content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Parts(ps) => ps.iter().filter_map(|p| {
                if let ContentPart::Text { text } = p { Some(text.as_str()) } else { None }
            }).collect::<Vec<_>>().join(" "),
        };
        if text.is_empty() { continue; }
        let truncated = if text.chars().count() > 200 {
            let idx = text.char_indices().nth(200).map(|(i, _)| i).unwrap_or(text.len());
            format!("{}...", &text[..idx])
        } else { text };
        parts.push(format!("{role}: {truncated}"));
    }
    if parts.is_empty() { return None; }
    Some(Message {
        role: Role::System,
        content: MessageContent::Text(
            format!("[Session summary before /clear]\n{}", parts.join("\n"))
        ),
    })
}

/// CJK-aware token estimate for a message (used by compaction threshold).
pub(crate) fn msg_tokens(m: &Message) -> usize {
    let text = match &m.content {
        MessageContent::Text(t) => t.as_str(),
        MessageContent::Parts(parts) => {
            return parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => estimate_tokens(text),
                    _ => 50,
                })
                .sum();
        }
    };
    estimate_tokens(text)
}

/// Trim session messages from oldest to newest so the total history fits
/// within the model's context budget.
///
/// Budget calculation:
///   reply_reserve  = max(context_budget * 20%, 2000)
///   system_tokens  = system_prompt.len() / 4
///   tools_tokens   = tools JSON size / 4
///   history_budget = context_budget - reply_reserve - system_tokens - tools_tokens
///
/// Always keeps at least the last 3 user-assistant pairs (6 messages).
pub(crate) fn apply_context_budget_trim(
    messages: &mut Vec<Message>,
    context_tokens: usize,
    system_prompt: &str,
    tools: &[ToolDef],
) {
    if messages.len() <= 6 {
        return;
    }

    let reply_reserve = (context_tokens / 5).max(2000);
    let sys_tokens = estimate_tokens(system_prompt);
    let tools_tokens = serde_json::to_string(tools)
        .map(|s| estimate_tokens(&s))
        .unwrap_or(0);

    let history_budget = context_tokens
        .saturating_sub(reply_reserve)
        .saturating_sub(sys_tokens)
        .saturating_sub(tools_tokens);

    let total_tokens: usize = messages.iter().map(msg_tokens).sum();
    if total_tokens <= history_budget {
        return;
    }

    // Trim from the front, keeping at least the last 6 messages.
    let min_keep = 6;
    let max_removable = messages.len().saturating_sub(min_keep);
    let mut removed_tokens: usize = 0;

    let mut remove_count = 0;
    for i in 0..max_removable {
        if total_tokens - removed_tokens <= history_budget {
            break;
        }
        removed_tokens += msg_tokens(&messages[i]);
        remove_count += 1;
    }

    if remove_count > 0 {
        tracing::info!(
            context_tokens,
            history_budget,
            total_tokens,
            removed = remove_count,
            remaining = messages.len() - remove_count,
            "context budget trim: removed {remove_count} oldest messages"
        );
        messages.drain(..remove_count);

        // Insert a system-like marker so the model knows history was truncated.
        messages.insert(0, Message {
            role: Role::User,
            content: MessageContent::Text(
                "[System: earlier conversation history was trimmed to fit context window. Continue naturally from the messages below.]".to_owned()
            ),
        });
        messages.insert(1, Message {
            role: Role::Assistant,
            content: MessageContent::Text("Understood.".to_owned()),
        });
    }
}

/// Compress an image for LLM: resize to max 1024px and convert to JPEG.
/// Uses the `image` crate (pure Rust, cross-platform).
/// Returns data URI or None if compression fails.
pub(crate) fn compress_image_for_llm(data_uri: &str) -> Option<String> {
    let b64 = data_uri
        .strip_prefix("data:image/png;base64,")
        .or_else(|| data_uri.strip_prefix("data:image/jpeg;base64,"))
        .or_else(|| data_uri.strip_prefix("data:image/webp;base64,"))
        .or_else(|| data_uri.strip_prefix("data:image/gif;base64,"))
        .unwrap_or(data_uri);

    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;

    // Skip if already small enough (<200KB)
    if bytes.len() < 200_000 {
        return Some(data_uri.to_owned());
    }

    let img = image::load_from_memory(&bytes).ok()?;

    // Resize so neither dimension exceeds 1024px, preserving aspect ratio.
    const MAX_DIM: u32 = 1024;
    let (w, h) = (img.width(), img.height());
    let img = if w > MAX_DIM || h > MAX_DIM {
        img.resize(MAX_DIM, MAX_DIM, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    // Encode to JPEG quality 85.
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Jpeg).ok()?;
    let compressed = buf.into_inner();

    let b64 = base64::engine::general_purpose::STANDARD.encode(&compressed);
    tracing::debug!(
        original = bytes.len(),
        compressed = compressed.len(),
        "image compressed for LLM"
    );
    Some(format!("data:image/jpeg;base64,{b64}"))
}
