//! Tool call argument repair for streaming JSON
//!
//! Some models (Kimi, Qwen, GLM, MiniMax) emit malformed JSON during streaming
//! where there's garbage text before the actual JSON arguments. This module
//! provides repair logic to extract usable arguments from such malformed
//! chunks.

use serde_json::Value;

/// Result of extracting usable tool call arguments from potentially malformed
/// input.
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

/// Extract a balanced JSON prefix from raw text that may have garbage
/// before/after. Returns (json_string, start_index) or None if no valid JSON
/// found.
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

/// Try to extract usable tool call arguments from raw text that may be
/// malformed.
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

    // NEW: If JSON parsing failed but we found a valid JSON boundary, try to fix
    // common issues This handles cases where model sends unescaped newlines in
    // string values
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
    // If the JSON structure looks valid (starts with { and has some key-value
    // pairs) but is truncated at the end, try to fix common truncation patterns
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
        let raw = r#"{"content": "line1
line2", "path": "test.rs"}"#;
        let result = try_extract_usable_args(raw);
        // This should extract the JSON despite unescaped newlines
        assert!(result.is_some());
        let repair = result.unwrap();
        assert_eq!(repair.kind, RepairKind::Repaired);
    }
}
