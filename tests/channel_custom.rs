//! Integration tests for the custom channel module.
//!
//! Tests public utility functions: `json_path_extract`, `parse_inbound`, and
//! the template/env-var expansion logic (via `parse_inbound` + webhook flow).

use rsclaw::channel::custom::{json_path_extract, parse_inbound};
use rsclaw::config::schema::CustomChannelConfig;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Helper to build a minimal CustomChannelConfig
// ---------------------------------------------------------------------------

fn cfg_with_paths(
    filter_path: Option<&str>,
    filter_value: Option<&str>,
    text_path: Option<&str>,
    sender_path: Option<&str>,
    group_path: Option<&str>,
) -> CustomChannelConfig {
    CustomChannelConfig {
        name: "test_custom".to_owned(),
        channel_type: "webhook".to_owned(),
        base: Default::default(),
        ws_url: None,
        ws_headers: None,
        auth_frame: None,
        auth_success_path: None,
        auth_success_value: None,
        heartbeat_interval: None,
        heartbeat_frame: None,
        filter_path: filter_path.map(|s| s.to_owned()),
        filter_value: filter_value.map(|s| s.to_owned()),
        text_path: text_path.map(|s| s.to_owned()),
        sender_path: sender_path.map(|s| s.to_owned()),
        group_path: group_path.map(|s| s.to_owned()),
        reply_url: None,
        reply_method: None,
        reply_template: None,
        reply_headers: None,
        reply_frame: None,
    }
}

// ---------------------------------------------------------------------------
// json_path_extract tests
// ---------------------------------------------------------------------------

/// Simple property access with leading `$.`
#[test]
fn json_path_simple_property() {
    let v: Value = json!({"status": "ok", "data": {"msg": "hi"}});
    assert_eq!(
        json_path_extract(&v, "$.status"),
        Some(&Value::String("ok".to_owned()))
    );
    assert_eq!(
        json_path_extract(&v, "$.data.msg"),
        Some(&Value::String("hi".to_owned()))
    );
}

/// Array indexing with `[n]` notation.
#[test]
fn json_path_array_indexing() {
    let v: Value = json!({"items": [{"id": 1}, {"id": 2}, {"id": 3}]});
    assert_eq!(
        json_path_extract(&v, "$.items[0].id"),
        Some(&Value::Number(1.into()))
    );
    assert_eq!(
        json_path_extract(&v, "$.items[2].id"),
        Some(&Value::Number(3.into()))
    );
    // Out-of-bounds returns None.
    assert!(json_path_extract(&v, "$.items[10].id").is_none());
}

/// Deeply nested access across multiple levels.
#[test]
fn json_path_deep_nesting() {
    let v: Value = json!({
        "a": {
            "b": {
                "c": {
                    "d": [
                        { "value": "deep" }
                    ]
                }
            }
        }
    });
    assert_eq!(
        json_path_extract(&v, "$.a.b.c.d[0].value"),
        Some(&Value::String("deep".to_owned()))
    );
}

/// Paths without the `$.` prefix should work the same.
#[test]
fn json_path_without_dollar_prefix() {
    let v: Value = json!({"x": 42});
    assert_eq!(
        json_path_extract(&v, "x"),
        Some(&Value::Number(42.into()))
    );
}

/// Empty path returns the root value.
#[test]
fn json_path_empty_returns_root() {
    let v: Value = json!({"a": 1});
    assert_eq!(json_path_extract(&v, "$."), Some(&v));
}

/// Non-existent path returns None.
#[test]
fn json_path_missing_returns_none() {
    let v: Value = json!({"a": 1});
    assert!(json_path_extract(&v, "$.b.c.d").is_none());
}

// ---------------------------------------------------------------------------
// parse_inbound tests
// ---------------------------------------------------------------------------

/// Basic parse_inbound with filter, text, and sender paths.
#[test]
fn parse_inbound_basic() {
    let cfg = cfg_with_paths(
        Some("$.event"),
        Some("msg"),
        Some("$.text"),
        Some("$.user"),
        None,
    );
    let body = r#"{"event":"msg","text":"hello world","user":"alice"}"#;
    let parsed = parse_inbound(&cfg, body).unwrap();
    assert_eq!(parsed.text, "hello world");
    assert_eq!(parsed.sender, "alice");
    assert!(parsed.group_id.is_none());
}

/// Filter mismatch returns None.
#[test]
fn parse_inbound_filter_mismatch() {
    let cfg = cfg_with_paths(
        Some("$.event"),
        Some("message"),
        Some("$.text"),
        None,
        None,
    );
    let body = r#"{"event":"heartbeat","text":"ping"}"#;
    assert!(parse_inbound(&cfg, body).is_none());
}

/// Group path extraction.
#[test]
fn parse_inbound_with_group() {
    let cfg = cfg_with_paths(
        None,
        None,
        Some("$.content"),
        Some("$.from"),
        Some("$.group"),
    );
    let body = r#"{"content":"hi","from":"bob","group":"room_1"}"#;
    let parsed = parse_inbound(&cfg, body).unwrap();
    assert_eq!(parsed.text, "hi");
    assert_eq!(parsed.sender, "bob");
    assert_eq!(parsed.group_id.as_deref(), Some("room_1"));
}

/// Missing text returns None (empty text filtered out).
#[test]
fn parse_inbound_empty_text_returns_none() {
    let cfg = cfg_with_paths(None, None, Some("$.text"), None, None);
    let body = r#"{"text":""}"#;
    assert!(parse_inbound(&cfg, body).is_none());
}

/// Invalid JSON returns None.
#[test]
fn parse_inbound_invalid_json() {
    let cfg = cfg_with_paths(None, None, Some("$.text"), None, None);
    assert!(parse_inbound(&cfg, "not json").is_none());
}

/// Without a sender_path, sender defaults to "unknown".
#[test]
fn parse_inbound_no_sender_defaults_to_unknown() {
    let cfg = cfg_with_paths(None, None, Some("$.msg"), None, None);
    let body = r#"{"msg":"test"}"#;
    let parsed = parse_inbound(&cfg, body).unwrap();
    assert_eq!(parsed.sender, "unknown");
}

/// Numeric values in JSON are stringified.
#[test]
fn parse_inbound_numeric_text_stringified() {
    let cfg = cfg_with_paths(None, None, Some("$.code"), None, None);
    let body = r#"{"code":42}"#;
    let parsed = parse_inbound(&cfg, body).unwrap();
    assert_eq!(parsed.text, "42");
}
