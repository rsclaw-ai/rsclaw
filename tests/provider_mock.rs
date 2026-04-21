//! Pure unit-style tests for provider construction and request types.
//!
//! No network calls, no wiremock. These verify that providers can be
//! constructed and that `LlmRequest` fields round-trip as expected.

use std::sync::Once;

static INIT_TLS: Once = Once::new();
fn init_tls() {
    INIT_TLS.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

use rsclaw::provider::{
    LlmProvider, LlmRequest, Message, MessageContent, Role, anthropic::AnthropicProvider,
    openai::OpenAiProvider,
};

// ---------------------------------------------------------------------------
// AnthropicProvider construction
// ---------------------------------------------------------------------------

#[test]
fn anthropic_provider_new_sets_name() {
    init_tls();
    let p = AnthropicProvider::new("sk-ant-test-key");
    assert_eq!(p.name(), "anthropic");
}

#[test]
fn anthropic_provider_with_base_url_sets_name() {
    init_tls();
    let p = AnthropicProvider::with_base_url("sk-ant-key", "http://localhost:4000");
    assert_eq!(p.name(), "anthropic");
}

// ---------------------------------------------------------------------------
// OpenAiProvider construction
// ---------------------------------------------------------------------------

#[test]
fn openai_provider_new_sets_name() {
    init_tls();
    let p = OpenAiProvider::new("sk-openai-key");
    assert_eq!(p.name(), "openai");
}

#[test]
fn openai_provider_without_key_sets_name() {
    init_tls();
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
        frequency_penalty: None,
        thinking_budget: None,
        kv_cache_mode: 0,
        session_key: None,
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
        frequency_penalty: None,
        thinking_budget: None,
        kv_cache_mode: 0,
        session_key: None,
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

// ---------------------------------------------------------------------------
// ContentPart serialization
// ---------------------------------------------------------------------------

#[test]
fn content_part_text_serialization() {
    use rsclaw::provider::ContentPart;
    let part = ContentPart::Text {
        text: "hello".to_owned(),
    };
    let v = serde_json::to_value(&part).expect("serialize");
    assert_eq!(v["type"].as_str().unwrap(), "text");
    assert_eq!(v["text"].as_str().unwrap(), "hello");
}

#[test]
fn content_part_image_serialization() {
    use rsclaw::provider::ContentPart;
    let part = ContentPart::Image {
        url: "https://example.com/img.png".to_owned(),
    };
    let v = serde_json::to_value(&part).expect("serialize");
    assert_eq!(v["type"].as_str().unwrap(), "image");
    assert_eq!(v["url"].as_str().unwrap(), "https://example.com/img.png");
}

#[test]
fn content_part_tool_use_serialization() {
    use rsclaw::provider::ContentPart;
    let part = ContentPart::ToolUse {
        id: "tu_123".to_owned(),
        name: "search".to_owned(),
        input: serde_json::json!({"query": "test"}),
    };
    let v = serde_json::to_value(&part).expect("serialize");
    assert_eq!(v["type"].as_str().unwrap(), "tool_use");
    assert_eq!(v["id"].as_str().unwrap(), "tu_123");
    assert_eq!(v["name"].as_str().unwrap(), "search");
    assert_eq!(v["input"]["query"].as_str().unwrap(), "test");
}

#[test]
fn content_part_tool_result_serialization() {
    use rsclaw::provider::ContentPart;
    let part = ContentPart::ToolResult {
        tool_use_id: "tu_123".to_owned(),
        content: "result data".to_owned(),
        is_error: Some(true),
    };
    let v = serde_json::to_value(&part).expect("serialize");
    assert_eq!(v["type"].as_str().unwrap(), "tool_result");
    assert_eq!(v["tool_use_id"].as_str().unwrap(), "tu_123");
    assert_eq!(v["content"].as_str().unwrap(), "result data");
    assert_eq!(v["is_error"].as_bool().unwrap(), true);
}

#[test]
fn content_part_tool_result_no_error() {
    use rsclaw::provider::ContentPart;
    let part = ContentPart::ToolResult {
        tool_use_id: "tu_456".to_owned(),
        content: "ok".to_owned(),
        is_error: None,
    };
    let v = serde_json::to_value(&part).expect("serialize");
    assert_eq!(v["type"].as_str().unwrap(), "tool_result");
    assert!(v["is_error"].is_null(), "is_error should be null when None");
}

// ---------------------------------------------------------------------------
// LlmRequest with tools, thinking_budget, clone independence
// ---------------------------------------------------------------------------

#[test]
fn llm_request_with_tools() {
    use rsclaw::provider::ToolDef;
    let req = LlmRequest {
        model: "gpt-4o".to_owned(),
        messages: vec![],
        tools: vec![
            ToolDef {
                name: "search".to_owned(),
                description: "Web search".to_owned(),
                parameters: serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            },
            ToolDef {
                name: "calc".to_owned(),
                description: "Calculator".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ],
        system: None,
        max_tokens: None,
        temperature: None,
        frequency_penalty: None,
        thinking_budget: None,
        kv_cache_mode: 0,
        session_key: None,
    };
    assert_eq!(req.tools.len(), 2);
    assert_eq!(req.tools[0].name, "search");
    assert_eq!(req.tools[1].name, "calc");
}

#[test]
fn llm_request_with_thinking_budget() {
    let req = LlmRequest {
        model: "claude-3-5-sonnet".to_owned(),
        messages: vec![],
        tools: vec![],
        system: None,
        max_tokens: None,
        temperature: None,
        frequency_penalty: None,
        thinking_budget: Some(10000),
        kv_cache_mode: 0,
        session_key: None,
    };
    assert_eq!(req.thinking_budget, Some(10000));
}

#[test]
fn llm_request_clone_independence() {
    let original = LlmRequest {
        model: "gpt-4o".to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("original".to_owned()),
        }],
        tools: vec![],
        system: Some("system prompt".to_owned()),
        max_tokens: Some(100),
        temperature: Some(0.5),
        frequency_penalty: None,
        thinking_budget: Some(5000),
        kv_cache_mode: 0,
        session_key: None,
    };

    let mut cloned = original.clone();
    cloned.model = "gpt-3.5-turbo".to_owned();
    cloned.max_tokens = Some(200);

    // Original should be unchanged
    assert_eq!(original.model, "gpt-4o");
    assert_eq!(original.max_tokens, Some(100));
    // Clone should have new values
    assert_eq!(cloned.model, "gpt-3.5-turbo");
    assert_eq!(cloned.max_tokens, Some(200));
}

// ---------------------------------------------------------------------------
// StreamEvent construction variants
// ---------------------------------------------------------------------------

#[test]
fn stream_event_text_delta() {
    use rsclaw::provider::StreamEvent;
    let event = StreamEvent::TextDelta("hello".to_owned());
    match event {
        StreamEvent::TextDelta(text) => assert_eq!(text, "hello"),
        _ => panic!("expected TextDelta"),
    }
}

#[test]
fn stream_event_reasoning_delta() {
    use rsclaw::provider::StreamEvent;
    let event = StreamEvent::ReasoningDelta("thinking...".to_owned());
    match event {
        StreamEvent::ReasoningDelta(text) => assert_eq!(text, "thinking..."),
        _ => panic!("expected ReasoningDelta"),
    }
}

#[test]
fn stream_event_tool_call() {
    use rsclaw::provider::StreamEvent;
    let event = StreamEvent::ToolCall {
        id: "tc_1".to_owned(),
        name: "search".to_owned(),
        input: serde_json::json!({"query": "test"}),
    };
    match event {
        StreamEvent::ToolCall { id, name, input } => {
            assert_eq!(id, "tc_1");
            assert_eq!(name, "search");
            assert_eq!(input["query"], "test");
        }
        _ => panic!("expected ToolCall"),
    }
}

#[test]
fn stream_event_done_with_usage() {
    use rsclaw::provider::{StreamEvent, TokenUsage};
    let event = StreamEvent::Done {
        usage: Some(TokenUsage {
            input: 100,
            output: 50,
        }),
    };
    match event {
        StreamEvent::Done { usage: Some(u) } => {
            assert_eq!(u.input, 100);
            assert_eq!(u.output, 50);
        }
        _ => panic!("expected Done with usage"),
    }
}

#[test]
fn stream_event_done_no_usage() {
    use rsclaw::provider::StreamEvent;
    let event = StreamEvent::Done { usage: None };
    match event {
        StreamEvent::Done { usage: None } => {}
        _ => panic!("expected Done with no usage"),
    }
}

#[test]
fn stream_event_error() {
    use rsclaw::provider::StreamEvent;
    let event = StreamEvent::Error("something went wrong".to_owned());
    match event {
        StreamEvent::Error(msg) => assert_eq!(msg, "something went wrong"),
        _ => panic!("expected Error"),
    }
}

// ---------------------------------------------------------------------------
// RetryConfig defaults and partial deserialization
// ---------------------------------------------------------------------------

#[test]
fn retry_config_defaults() {
    use rsclaw::provider::RetryConfig;
    let cfg = RetryConfig::default();
    assert_eq!(cfg.attempts, 3);
    assert_eq!(cfg.min_delay_ms, 400);
    assert_eq!(cfg.max_delay_ms, 30_000);
    assert!((cfg.jitter - 0.1).abs() < 1e-6);
}

#[test]
fn retry_config_partial_deserialization() {
    use rsclaw::provider::RetryConfig;
    // Only override attempts, rest should use defaults
    let json = r#"{"attempts": 5}"#;
    let cfg: RetryConfig = serde_json::from_str(json).expect("deserialize");
    assert_eq!(cfg.attempts, 5);
    assert_eq!(cfg.min_delay_ms, 400); // default
    assert_eq!(cfg.max_delay_ms, 30_000); // default
    assert!((cfg.jitter - 0.1).abs() < 1e-6); // default
}

#[test]
fn retry_config_full_deserialization() {
    use rsclaw::provider::RetryConfig;
    let json = r#"{"attempts": 10, "min_delay_ms": 100, "max_delay_ms": 60000, "jitter": 0.2}"#;
    let cfg: RetryConfig = serde_json::from_str(json).expect("deserialize");
    assert_eq!(cfg.attempts, 10);
    assert_eq!(cfg.min_delay_ms, 100);
    assert_eq!(cfg.max_delay_ms, 60_000);
    assert!((cfg.jitter - 0.2).abs() < 1e-6);
}
