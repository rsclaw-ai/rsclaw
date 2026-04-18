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
            // Remove Tool messages, but also check for orphaned Assistants
            // An Assistant with tool_calls whose Tool results were removed should also be removed
            let mut assistant_to_remove: Vec<usize> = Vec::new();
            for i in &to_remove {
                // Check if preceding message is Assistant with tool_calls
                if *i > 0 {
                    let prev_idx = i - 1;
                    if messages[prev_idx].role == Role::Assistant {
                        if let MessageContent::Parts(parts) = &messages[prev_idx].content {
                            let has_tool_calls = parts.iter().any(|p| matches!(p, ContentPart::ToolUse { .. }));
                            if has_tool_calls {
                                // This Assistant's Tool result is being removed, mark it too
                                if !to_remove.contains(&prev_idx) && !assistant_to_remove.contains(&prev_idx) {
                                    assistant_to_remove.push(prev_idx);
                                }
                            }
                        }
                    }
                }
            }
            // Combine and sort removal indices
            to_remove.extend(assistant_to_remove);
            to_remove.sort();
            to_remove.dedup();

            for i in to_remove.into_iter().rev() {
                messages.remove(i);
            }

            // Final check: if first message is Tool, remove it (orphaned)
            while !messages.is_empty() && messages[0].role == Role::Tool {
                messages.remove(0);
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
/// IMPORTANT: Ensures no orphaned Tool messages (Tool must follow Assistant with tool_calls).
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
    // CRITICAL: Never leave orphaned Tool messages at the start.
    // A Tool message must follow an Assistant with tool_calls.
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

    // Adjust remove_count to avoid orphaned Tool message at start.
    // If the first remaining message would be Tool, extend removal to include it.
    if remove_count < messages.len() {
        let first_remaining_idx = remove_count;
        if messages[first_remaining_idx].role == Role::Tool {
            // Need to remove this Tool and its preceding Assistant (tool_calls)
            // Find the Assistant that has tool_calls before this Tool
            // Actually, we should remove until we hit a non-Tool message
            while remove_count < messages.len() && messages[remove_count].role == Role::Tool {
                removed_tokens += msg_tokens(&messages[remove_count]);
                remove_count += 1;
            }
            // Also remove any Assistant that follows (it might have tool_calls for the removed Tool)
            // Actually, we need to find a safe starting point: either User or System
            // Skip any Assistant that might be incomplete (has tool_calls for removed Tools)
            while remove_count < messages.len() {
                let msg = &messages[remove_count];
                if msg.role == Role::User || msg.role == Role::System {
                    break;
                }
                // Check if Assistant has tool_calls - if so, its Tool results may have been removed
                if msg.role == Role::Assistant {
                    if let MessageContent::Parts(parts) = &msg.content {
                        let has_tool_calls = parts.iter().any(|p| matches!(p, ContentPart::ToolUse { .. }));
                        if has_tool_calls {
                            // This Assistant's Tool results were removed, remove it too
                            removed_tokens += msg_tokens(msg);
                            remove_count += 1;
                            continue;
                        }
                    }
                }
                // Non-tool-call Assistant is safe, stop
                break;
            }
        }
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

/// Validate and repair message sequence to ensure no orphaned Tool messages.
/// OpenAI API requires Tool messages to follow Assistant messages with tool_calls.
/// Returns the number of messages removed.
pub(crate) fn validate_message_sequence(messages: &mut Vec<Message>) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let mut removed = 0;

    // Check first message - cannot be Tool
    while !messages.is_empty() && messages[0].role == Role::Tool {
        tracing::warn!("validate_message_sequence: removing orphaned Tool at start");
        messages.remove(0);
        removed += 1;
    }

    // Scan through and check each Tool message has preceding Assistant with tool_calls
    let mut i = 1;
    while i < messages.len() {
        if messages[i].role == Role::Tool {
            // Check preceding message
            let prev = &messages[i - 1];
            let has_tool_calls = match &prev.content {
                MessageContent::Parts(parts) => parts.iter().any(|p| matches!(p, ContentPart::ToolUse { .. })),
                _ => false,
            };

            if prev.role != Role::Assistant || !has_tool_calls {
                // Orphaned Tool - remove it and any following Tools
                tracing::warn!(
                    idx = i,
                    "validate_message_sequence: removing orphaned Tool without preceding Assistant(tool_calls)"
                );
                messages.remove(i);
                removed += 1;
                continue; // Don't increment i, check same position again
            }
        }
        i += 1;
    }

    // Final check: Assistant with tool_calls should have Tool results
    // If an Assistant has tool_calls but no following Tool results, remove its tool_calls
    // (This is less common, but let's handle it)
    for i in 0..messages.len() {
        if messages[i].role == Role::Assistant {
            if let MessageContent::Parts(parts) = &mut messages[i].content {
                let has_tool_calls = parts.iter().any(|p| matches!(p, ContentPart::ToolUse { .. }));
                if has_tool_calls {
                    // Check if next message(s) are Tool results for these calls
                    let tool_call_ids: Vec<String> = parts
                        .iter()
                        .filter_map(|p| {
                            if let ContentPart::ToolUse { id, .. } = p {
                                Some(id.clone())
                            } else {
                                None
                            }
                        })
                        .collect();

                    // Find Tool results in following messages
                    let mut found_results: std::collections::HashSet<String> = std::collections::HashSet::new();
                    for j in (i + 1)..messages.len() {
                        if messages[j].role == Role::User {
                            break; // Stop at next User message
                        }
                        if messages[j].role == Role::Tool {
                            if let MessageContent::Parts(result_parts) = &messages[j].content {
                                for p in result_parts {
                                    if let ContentPart::ToolResult { tool_use_id, .. } = p {
                                        found_results.insert(tool_use_id.clone());
                                    }
                                }
                            }
                        }
                    }

                    // If some tool_calls have no results, we should probably remove them
                    // But this is complex - for now just log a warning
                    let missing = tool_call_ids.iter().filter(|id| !found_results.contains(id)).count();
                    if missing > 0 {
                        tracing::warn!(
                            idx = i,
                            missing = missing,
                            "validate_message_sequence: Assistant has tool_calls without matching Tool results"
                        );
                    }
                }
            }
        }
    }

    removed
}
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
