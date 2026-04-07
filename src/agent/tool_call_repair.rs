//! Tool call argument repair for streaming JSON and transcript repair
//!
//! Some models (Kimi, Qwen, GLM, MiniMax) emit malformed JSON during streaming
//! where there's garbage text before the actual JSON arguments. This module
//! provides repair logic to extract usable arguments from such malformed chunks.
//!
//! Also provides transcript repair to ensure assistant messages with tool_calls
//! are properly paired with tool_result messages, fixing orphaned tool_calls
//! that would cause API errors.

use crate::provider::{ContentPart, Message, MessageContent, Role};
use serde_json::Value;

/// Result of extracting usable tool call arguments from potentially malformed input.
#[derive(Debug)]
pub struct ToolCallRepair {
    pub args: Value,
    pub kind: RepairKind,
    pub leading_prefix: String,
    pub trailing_suffix: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RepairKind {
    /// Arguments were already valid JSON and preserved as-is.
    Preserved,
    /// Arguments were repaired by extracting valid JSON from malformed text.
    Repaired,
}

/// Extract a balanced JSON prefix from raw text that may have garbage before/after.
/// Returns (json_string, start_index) or None if no valid JSON found.
pub fn extract_balanced_json_prefix(raw: &str) -> Option<(String, usize)> {
    let mut start = 0;
    while start < raw.len() {
        let c = raw[start..].chars().next()?;
        if c == '{' || c == '[' {
            break;
        }
        start += 1;
    }
    if start >= raw.len() {
        return None;
    }

    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in raw[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            continue;
        }
        if c == '{' || c == '[' {
            depth += 1;
            continue;
        }
        if c == '}' || c == ']' {
            depth -= 1;
            if depth == 0 {
                let json_end = start + i + 1;
                return Some((raw[start..json_end].to_owned(), start));
            }
        }
    }
    None
}

/// Check if we should attempt repair based on the delta content.
#[allow(dead_code)]
pub fn should_attempt_repair(partial_json: &str, delta: &str) -> bool {
    // If delta contains closing brackets, worth trying to repair
    if delta.contains('}') || delta.contains(']') {
        return true;
    }
    let trimmed = delta.trim();
    // Small trailing text that ends with } or ] is a good candidate
    trimmed.len() <= 3 && (partial_json.contains('}') || partial_json.contains(']'))
}

/// Check if the leading prefix is allowed (not garbage text).
fn is_allowed_leading_prefix(prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    // Max 96 chars
    if prefix.len() > 96 {
        return false;
    }
    // Only allow alphanumeric, whitespace, and common punctuation
    if let Ok(re) = regex::Regex::new(r#"^[a-z0-9\s"'`.:/_\\-]+$"#) {
        if !re.is_match(prefix) {
            return false;
        }
    }
    // Allow if it's small enough and doesn't look like random garbage
    // or if it starts with function/tools prefix
    let first_char = prefix.chars().next().unwrap_or(' ');
    prefix.len() <= 10
        || first_char == '.'
        || first_char == ':'
        || first_char == '"'
        || first_char == '`'
        || prefix.to_lowercase().starts_with("functions")
        || prefix.to_lowercase().starts_with("tools")
        || prefix.starts_with("function")
        || prefix.starts_with("tool")
}

/// Check if trailing suffix is allowed (small, not garbage).
fn is_allowed_trailing_suffix(suffix: &str) -> bool {
    if suffix.is_empty() {
        return true;
    }
    // Max 3 chars
    if suffix.len() > 3 {
        return false;
    }
    // No whitespace, braces, brackets, quotes, or backslash
    !suffix.chars().any(|c| {
        c.is_whitespace() || c == '{' || c == '[' || c == '}' || c == ']' || c == '"' || c == '\\'
    })
}

/// Try to extract usable tool call arguments from raw text that may be malformed.
pub fn try_extract_usable_args(raw: &str) -> Option<ToolCallRepair> {
    if raw.trim().is_empty() {
        return None;
    }

    // First, try parsing as-is (preserving if valid)
    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        if parsed.is_object() {
            return Some(ToolCallRepair {
                args: parsed,
                kind: RepairKind::Preserved,
                leading_prefix: String::new(),
                trailing_suffix: String::new(),
            });
        }
    }

    // Try to extract balanced JSON prefix
    let extracted = extract_balanced_json_prefix(raw)?;
    let leading_prefix = raw[..extracted.1].trim().to_string();
    let json_part = &extracted.0;
    let trailing_suffix = raw[extracted.1 + json_part.len()..].trim().to_string();

    // Validate leading prefix - be more lenient for incomplete JSON
    if !leading_prefix.is_empty() && !is_allowed_leading_prefix(&leading_prefix) {
        return None;
    }

    // Validate trailing suffix - be lenient if JSON is incomplete
    if !leading_prefix.is_empty() && !trailing_suffix.is_empty() {
        if !is_allowed_trailing_suffix(&trailing_suffix) {
            return None;
        }
    }

    // Try to parse the extracted JSON
    if let Ok(parsed) = serde_json::from_str::<Value>(json_part) {
        if parsed.is_object() {
            return Some(ToolCallRepair {
                args: parsed,
                kind: RepairKind::Repaired,
                leading_prefix,
                trailing_suffix,
            });
        }
    }

    // NEW: If JSON parsing failed but we found a valid JSON boundary, try to fix common issues
    // This handles cases where model sends unescaped newlines in string values
    if !json_part.is_empty() {
        // Try escaping common issues and re-parse
        let fixed = json_part
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");

        if let Ok(parsed) = serde_json::from_str::<Value>(&fixed) {
            if parsed.is_object() {
                return Some(ToolCallRepair {
                    args: parsed,
                    kind: RepairKind::Repaired,
                    leading_prefix,
                    trailing_suffix,
                });
            }
        }
    }

    // NEW: Handle INCOMPLETE but otherwise valid JSON
    // If the JSON structure looks valid (starts with { and has some key-value pairs)
    // but is truncated at the end, try to fix common truncation patterns
    if json_part.starts_with('{') && json_part.contains(':') {
        // Try to find the last complete key-value pair and close the JSON
        let incomplete = json_part.to_string();

        // Find the last complete value (string or number)
        // Look for patterns like: "key": "value" or "key": number
        let fixed_incomplete = fix_incomplete_json(&incomplete);
        if let Ok(parsed) = serde_json::from_str::<Value>(&fixed_incomplete) {
            if parsed.is_object() && !parsed.as_object().unwrap().is_empty() {
                return Some(ToolCallRepair {
                    args: parsed,
                    kind: RepairKind::Repaired,
                    leading_prefix,
                    trailing_suffix: String::new(), // Already handled in fixed_incomplete
                });
            }
        }
    }

    None
}

/// Try to fix incomplete JSON by completing the structure
fn fix_incomplete_json(incomplete: &str) -> String {
    let trimmed = incomplete.trim();
    if trimmed.is_empty() {
        return "{}".to_string();
    }

    // If already valid, return as-is
    if serde_json::from_str::<Value>(trimmed).is_ok() {
        return trimmed.to_string();
    }

    // If it ends with a complete value, add closing brace
    let last_char = trimmed.chars().last().unwrap_or(' ');
    if last_char == '"'
        || last_char.is_ascii_digit()
        || last_char == 'n'
        || last_char == 'f'
        || last_char == 't'
    {
        // Likely complete, just missing the closing brace
        return format!("{}}}", trimmed);
    }

    // Try adding closing braces/brackets
    let mut result = trimmed.to_string();
    let mut depth = 0;
    for c in trimmed.chars() {
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            _ => {}
        }
    }
    // Close any remaining open structures
    while depth > 0 {
        result.push('}');
        depth -= 1;
    }

    result
}

// ---------------------------------------------------------------------------
// Transcript repair: ensure tool_calls are paired with tool_results
// ---------------------------------------------------------------------------

/// Result of transcript repair, containing repaired messages and synthetic messages
/// that need to be persisted to session storage.
#[derive(Debug)]
pub struct RepairResult {
    /// The repaired message list (for LLM request)
    pub messages: Vec<Message>,
    /// Synthetic tool result messages that were added (need to be persisted)
    pub synthetic_messages: Vec<Message>,
}

/// Repair session transcript to ensure all assistant tool_calls have matching
/// tool_result messages. This prevents API errors from orphaned tool_calls.
///
/// OpenAI-compatible APIs require that every assistant message with tool_calls
/// is followed by tool messages responding to each tool_call_id. If a session
/// was interrupted mid-tool-execution, some tool_calls may lack results.
///
/// This function:
/// 1. Scans all assistant messages for ToolUse parts
/// 2. Collects all tool_use_ids from those messages
/// 3. Ensures each tool_use_id has at least one ToolResult
/// 4. Inserts synthetic error ToolResults for missing ids
/// 5. Removes orphaned ToolResult messages (no matching ToolUse)
///
/// Returns a `RepairResult` with repaired messages and synthetic messages to persist.
pub fn repair_tool_result_pairing(messages: Vec<Message>) -> RepairResult {
    // Collect all tool_use_ids from assistant messages
    let mut all_tool_ids: Vec<String> = Vec::new();
    for msg in &messages {
        if msg.role != Role::Assistant {
            continue;
        }
        match &msg.content {
            MessageContent::Parts(parts) => {
                for part in parts {
                    if let ContentPart::ToolUse { id, .. } = part {
                        all_tool_ids.push(id.clone());
                    }
                }
            }
            _ => {}
        }
    }

    // Collect tool_use_ids that have results
    let mut has_result: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in &messages {
        if msg.role != Role::Tool {
            continue;
        }
        match &msg.content {
            MessageContent::Parts(parts) => {
                for part in parts {
                    if let ContentPart::ToolResult { tool_use_id, .. } = part {
                        has_result.insert(tool_use_id.clone());
                    }
                }
            }
            _ => {}
        }
    }

    // Find missing tool_use_ids
    let missing_ids: Vec<String> = all_tool_ids
        .iter()
        .filter(|id| !has_result.contains(*id))
        .cloned()
        .collect();

    // Build repaired messages
    let mut repaired: Vec<Message> = Vec::new();
    let mut synthetic_messages: Vec<Message> = Vec::new();

    for msg in messages {
        // Check if this is an orphaned ToolResult (tool_use_id not in all_tool_ids)
        if msg.role == Role::Tool {
            let is_orphan = match &msg.content {
                MessageContent::Parts(parts) => {
                    parts.iter().all(|part| {
                        if let ContentPart::ToolResult { tool_use_id, .. } = part {
                            !all_tool_ids.contains(tool_use_id)
                        } else {
                            false
                        }
                    })
                }
                _ => false,
            };
            if is_orphan {
                tracing::warn!(
                    "repair_tool_result_pairing: removing orphaned tool result for unknown tool_call_id"
                );
                continue; // Skip orphaned tool result
            }
        }

        // Extract tool_ids from this message BEFORE pushing (to avoid borrow-after-move)
        let tool_ids_in_this_msg: Vec<String> = if msg.role == Role::Assistant {
            match &msg.content {
                MessageContent::Parts(parts) => {
                    parts
                        .iter()
                        .filter_map(|part| {
                            if let ContentPart::ToolUse { id, .. } = part {
                                Some(id.clone())
                            } else {
                                None
                            }
                        })
                        .collect()
                }
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };

        repaired.push(msg);

        // After assistant message, add synthetic ToolResults for missing ids
        for missing_id in tool_ids_in_this_msg.iter().filter(|id| missing_ids.contains(id)) {
            tracing::warn!(
                tool_call_id = %missing_id,
                "repair_tool_result_pairing: adding synthetic error result for missing tool_call_id"
            );
            let synthetic = Message {
                role: Role::Tool,
                content: MessageContent::Parts(vec![
                    ContentPart::ToolResult {
                        tool_use_id: missing_id.clone(),
                        content: "[Session interrupted: tool execution was not completed]".to_owned(),
                        is_error: Some(true),
                    },
                ]),
            };
            repaired.push(synthetic.clone());
            synthetic_messages.push(synthetic);
        }
    }

    RepairResult {
        messages: repaired,
        synthetic_messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_balanced_json_simple() {
        let result = extract_balanced_json_prefix(r#"hello {"key": "value"} world"#);
        assert!(result.is_some());
        let (json, start) = result.unwrap();
        assert_eq!(json, r#"{"key": "value"}"#);
        assert_eq!(start, 6);
    }

    #[test]
    fn test_extract_balanced_json_nested() {
        let result = extract_balanced_json_prefix(r#"garbage {"a": [1, 2, {"b": true}]} tail"#);
        assert!(result.is_some());
        let (json, _) = result.unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["a"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_try_extract_usable_args_valid() {
        let result = try_extract_usable_args(r#"{"content": "hello"}"#);
        assert!(result.is_some());
        let repair = result.unwrap();
        assert_eq!(repair.kind, RepairKind::Preserved);
        assert_eq!(repair.args["content"], "hello");
    }

    #[test]
    fn test_try_extract_usable_args_with_garbage() {
        // Test with minimal leading prefix and short trailing suffix
        let result = try_extract_usable_args(r#"abc {"content": "hello"} ab"#);
        assert!(result.is_some());
        let repair = result.unwrap();
        assert_eq!(repair.kind, RepairKind::Repaired);
        assert_eq!(repair.args["content"], "hello");
        assert_eq!(repair.leading_prefix, "abc");
        assert_eq!(repair.trailing_suffix, "ab");
    }

    #[test]
    fn test_should_attempt_repair() {
        assert!(should_attempt_repair(r#"{"key"#, "}"));
        assert!(!should_attempt_repair(r#"{""#, "a"));
        assert!(should_attempt_repair(r#"{""#, "x}"));
    }

    #[test]
    fn test_try_extract_usable_args_with_unescaped_newlines() {
        // Test case where JSON has unescaped newlines in string values
        let raw = "{\"content\": \"line1\nline2\", \"path\": \"test.rs\"}";
        let result = try_extract_usable_args(raw);
        // This should extract the JSON despite unescaped newlines
        assert!(result.is_some());
        let repair = result.unwrap();
        assert_eq!(repair.kind, RepairKind::Repaired);
    }

    // ---------------------------------------------------------------------------
    // Transcript repair tests
    // ---------------------------------------------------------------------------

    #[test]
    fn test_repair_missing_tool_result() {
        // Assistant with ToolUse but no ToolResult
        let messages = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("hello".to_owned()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::ToolUse {
                        id: "call_123".to_owned(),
                        name: "test_tool".to_owned(),
                        input: serde_json::json!({"arg": "value"}),
                    },
                ]),
            },
        ];

        let result = repair_tool_result_pairing(messages);
        let repaired = &result.messages;

        // Should have 3 messages: user, assistant, synthetic tool result
        assert_eq!(repaired.len(), 3);
        assert_eq!(repaired[0].role, Role::User);
        assert_eq!(repaired[1].role, Role::Assistant);
        assert_eq!(repaired[2].role, Role::Tool);

        // Check synthetic tool result
        match &repaired[2].content {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 1);
                match &parts[0] {
                    ContentPart::ToolResult { tool_use_id, content, is_error } => {
                        assert_eq!(tool_use_id, "call_123");
                        assert!(content.contains("interrupted"));
                        assert_eq!(*is_error, Some(true));
                    }
                    _ => panic!("Expected ToolResult"),
                }
            }
            _ => panic!("Expected Parts"),
        }

        // Should have 1 synthetic message
        assert_eq!(result.synthetic_messages.len(), 1);
    }

    #[test]
    fn test_repair_complete_pairing() {
        // Already properly paired
        let messages = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("hello".to_owned()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::ToolUse {
                        id: "call_123".to_owned(),
                        name: "test_tool".to_owned(),
                        input: serde_json::json!({}),
                    },
                ]),
            },
            Message {
                role: Role::Tool,
                content: MessageContent::Parts(vec![
                    ContentPart::ToolResult {
                        tool_use_id: "call_123".to_owned(),
                        content: "result".to_owned(),
                        is_error: Some(false),
                    },
                ]),
            },
        ];

        let result = repair_tool_result_pairing(messages);
        assert_eq!(result.messages.len(), 3);
        // Should be unchanged
        assert_eq!(result.messages[2].role, Role::Tool);
        // No synthetic messages added
        assert!(result.synthetic_messages.is_empty());
    }

    #[test]
    fn test_repair_multiple_tool_calls() {
        // Assistant with two ToolUse, only one has ToolResult
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::ToolUse {
                        id: "call_1".to_owned(),
                        name: "tool_a".to_owned(),
                        input: serde_json::json!({}),
                    },
                    ContentPart::ToolUse {
                        id: "call_2".to_owned(),
                        name: "tool_b".to_owned(),
                        input: serde_json::json!({}),
                    },
                ]),
            },
            Message {
                role: Role::Tool,
                content: MessageContent::Parts(vec![
                    ContentPart::ToolResult {
                        tool_use_id: "call_1".to_owned(),
                        content: "result 1".to_owned(),
                        is_error: Some(false),
                    },
                ]),
            },
        ];

        let result = repair_tool_result_pairing(messages);
        let repaired = &result.messages;

        // Should have 3 messages: assistant + synthetic for call_2 + existing for call_1
        assert_eq!(repaired.len(), 3);
        assert_eq!(repaired[0].role, Role::Assistant);
        assert_eq!(repaired[1].role, Role::Tool);
        assert_eq!(repaired[2].role, Role::Tool);

        // Synthetic result for call_2 should be immediately after assistant (position 1)
        match &repaired[1].content {
            MessageContent::Parts(parts) => match &parts[0] {
                ContentPart::ToolResult { tool_use_id, content, is_error } => {
                    assert_eq!(tool_use_id, "call_2");
                    assert!(content.contains("interrupted"));
                    assert_eq!(*is_error, Some(true));
                }
                _ => panic!("Expected ToolResult"),
            },
            _ => panic!("Expected Parts"),
        }

        // Existing result for call_1 should be at position 2
        match &repaired[2].content {
            MessageContent::Parts(parts) => match &parts[0] {
                ContentPart::ToolResult { tool_use_id, content, is_error } => {
                    assert_eq!(tool_use_id, "call_1");
                    assert_eq!(content, "result 1");
                    assert_eq!(*is_error, Some(false));
                }
                _ => panic!("Expected ToolResult"),
            },
            _ => panic!("Expected Parts"),
        }

        // Should have 1 synthetic message (for call_2)
        assert_eq!(result.synthetic_messages.len(), 1);
    }

    #[test]
    fn test_remove_orphaned_tool_result() {
        // ToolResult without matching ToolUse
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("just text".to_owned()),
            },
            Message {
                role: Role::Tool,
                content: MessageContent::Parts(vec![
                    ContentPart::ToolResult {
                        tool_use_id: "orphan_123".to_owned(),
                        content: "orphaned result".to_owned(),
                        is_error: Some(false),
                    },
                ]),
            },
        ];

        let result = repair_tool_result_pairing(messages);

        // Orphaned ToolResult should be removed
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].role, Role::Assistant);
        // No synthetic messages added
        assert!(result.synthetic_messages.is_empty());
    }
}