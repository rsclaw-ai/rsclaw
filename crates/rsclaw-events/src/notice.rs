//! Turns the runtime starts on its own, which history readers hide.
//!
//! When an async job finishes, the runtime wakes the session's agent with
//! an automatic notice (a user-role message the user never typed). If
//! nothing is left to do the agent answers `NO_REPLY`, which never reaches
//! the user. Both stay in the stored history so the model keeps a coherent
//! transcript, and are filtered out wherever history is shown to people.

use serde_json::Value;

/// The sentence every automatic notice carries; how readers recognize one.
pub const AUTO_NOTICE: &str = "This is an automatic notice, not a message from the user.";

/// The reply that means "nothing to say".
pub const NO_REPLY: &str = "NO_REPLY";

/// Whether `text` is an automatic notice.
pub fn is_auto_notice(text: &str) -> bool {
    text.contains(AUTO_NOTICE)
}

/// Whether `text` is a `NO_REPLY` answer.
pub fn is_no_reply(text: &str) -> bool {
    text.trim_start().to_ascii_uppercase().starts_with(NO_REPLY)
}

/// Whether streamed `text` is, or could still turn into, a `NO_REPLY`
/// answer. Text inside an unclosed `<think>` block is not an answer yet.
pub fn may_become_no_reply(text: &str) -> bool {
    let visible = match text.rfind("</think>") {
        Some(i) => &text[i + "</think>".len()..],
        None if text.contains("<think>") => return false,
        None => text,
    };
    let visible = visible.trim_start().to_ascii_uppercase();
    visible.starts_with(NO_REPLY) || NO_REPLY.starts_with(visible.as_str())
}

/// Whether a stored history message is an automatic notice or a `NO_REPLY`
/// answer, neither of which belongs in a history shown to people.
pub fn is_hidden_message(msg: &Value) -> bool {
    let text = match msg.get("content") {
        Some(Value::String(s)) => s.as_str(),
        _ => return false,
    };
    match msg.get("role").and_then(Value::as_str) {
        Some("user") => is_auto_notice(text),
        Some("assistant") => is_no_reply(text),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notices_and_no_replies_are_hidden() {
        let notice = format!("Now: 2026-10-27 16:11 Tue +08:00\n[image generation complete] job x: p\n{AUTO_NOTICE} ...");
        assert!(is_hidden_message(&json!({"role": "user", "content": notice})));
        assert!(is_hidden_message(&json!({"role": "assistant", "content": "NO_REPLY"})));
        assert!(is_hidden_message(&json!({"role": "assistant", "content": " no_reply\n"})));
        assert!(!is_hidden_message(&json!({"role": "assistant", "content": "图片已生成"})));
        assert!(!is_hidden_message(&json!({"role": "user", "content": "NO_REPLY"})));
    }

    #[test]
    fn streamed_prefixes_of_no_reply_are_held() {
        assert!(may_become_no_reply(""));
        assert!(may_become_no_reply("NO_"));
        assert!(may_become_no_reply("\nNO_REPLY"));
        assert!(may_become_no_reply("<think>x</think>NO"));
        assert!(!may_become_no_reply("<think>still thinking"));
        assert!(!may_become_no_reply("NOT"));
        assert!(!may_become_no_reply("好的"));
    }
}
