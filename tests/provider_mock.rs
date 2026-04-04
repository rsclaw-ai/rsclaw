//! Pure unit-style tests for provider construction and request types.
//!
//! No network calls, no wiremock. These verify that providers can be
//! constructed and that `LlmRequest` fields round-trip as expected.

use rsclaw::provider::{
    LlmProvider, LlmRequest, Message, MessageContent, Role, anthropic::AnthropicProvider,
    openai::OpenAiProvider,
};

// ---------------------------------------------------------------------------
// AnthropicProvider construction
// ---------------------------------------------------------------------------

#[test]
fn anthropic_provider_new_sets_name() {
    let p = AnthropicProvider::new("sk-ant-test-key");
    assert_eq!(p.name(), "anthropic");
}

#[test]
fn anthropic_provider_with_base_url_sets_name() {
    let p = AnthropicProvider::with_base_url("sk-ant-key", "http://localhost:4000");
    assert_eq!(p.name(), "anthropic");
}

// ---------------------------------------------------------------------------
// OpenAiProvider construction
// ---------------------------------------------------------------------------

#[test]
fn openai_provider_new_sets_name() {
    let p = OpenAiProvider::new("sk-openai-key");
    assert_eq!(p.name(), "openai");
}

#[test]
fn openai_provider_without_key_sets_name() {
    let p = OpenAiProvider::with_base_url("http://localhost:11434", None);
    assert_eq!(p.name(), "openai");
}

// ---------------------------------------------------------------------------
// LlmRequest field construction
// ---------------------------------------------------------------------------

#[test]
fn llm_request_fields_are_accessible() {
    let req = LlmRequest {
        model: "claude-3-5-sonnet-20241022".to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hello".to_owned()),
        }],
        tools: vec![],
        system: Some("You are helpful.".to_owned()),
        max_tokens: Some(1024),
        temperature: Some(0.5),
        thinking_budget: None,
    };

    assert_eq!(req.model, "claude-3-5-sonnet-20241022");
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, Role::User);
    assert_eq!(req.system.as_deref(), Some("You are helpful."));
    assert_eq!(req.max_tokens, Some(1024));
    assert!((req.temperature.unwrap() - 0.5).abs() < 1e-4);
}

#[test]
fn llm_request_defaults_are_none() {
    let req = LlmRequest {
        model: "gpt-4o".to_owned(),
        messages: vec![],
        tools: vec![],
        system: None,
        max_tokens: None,
        temperature: None,
        thinking_budget: None,
    };

    assert!(req.system.is_none());
    assert!(req.max_tokens.is_none());
    assert!(req.temperature.is_none());
    assert!(req.tools.is_empty());
}

#[test]
fn message_role_serializes_to_lowercase() {
    // Role derives Serialize with rename_all = "lowercase", verify via serde_json.
    let msg = Message {
        role: Role::Assistant,
        content: MessageContent::Text("hi".to_owned()),
    };
    let v = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(v["role"].as_str().unwrap(), "assistant");

    let msg2 = Message {
        role: Role::System,
        content: MessageContent::Text("sys".to_owned()),
    };
    let v2 = serde_json::to_value(&msg2).expect("serialize");
    assert_eq!(v2["role"].as_str().unwrap(), "system");
}
