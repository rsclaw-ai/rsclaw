//! A2A v1.0 wire-format compliance tests.

use rsclaw::a2a::types::{A2aPart, TaskState};
use serde_json::json;

#[test]
fn task_state_serializes_to_v1_screaming_snake() {
    let cases = [
        (TaskState::Unspecified,    "\"TASK_STATE_UNSPECIFIED\""),
        (TaskState::Submitted,      "\"TASK_STATE_SUBMITTED\""),
        (TaskState::Working,        "\"TASK_STATE_WORKING\""),
        (TaskState::Completed,      "\"TASK_STATE_COMPLETED\""),
        (TaskState::Failed,         "\"TASK_STATE_FAILED\""),
        (TaskState::Canceled,       "\"TASK_STATE_CANCELED\""),
        (TaskState::InputRequired,  "\"TASK_STATE_INPUT_REQUIRED\""),
        (TaskState::AuthRequired,   "\"TASK_STATE_AUTH_REQUIRED\""),
        (TaskState::Rejected,       "\"TASK_STATE_REJECTED\""),
    ];
    for (state, expected) in cases {
        assert_eq!(serde_json::to_string(&state).unwrap(), expected, "{state:?}");
    }
}

#[test]
fn part_text_serializes_with_type_tag() {
    let p = A2aPart::Text { text: "hi".into() };
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v, json!({ "type": "text", "text": "hi" }));
}

#[test]
fn part_raw_carries_bytes_as_base64() {
    let p = A2aPart::Raw {
        bytes: "aGVsbG8=".into(),
        mime_type: "application/octet-stream".into(),
    };
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["type"], "raw");
    assert_eq!(v["bytes"], "aGVsbG8=");
    assert_eq!(v["mimeType"], "application/octet-stream");
}

#[test]
fn part_url_carries_reference() {
    let p = A2aPart::Url {
        url: "https://example.com/x.png".into(),
        mime_type: Some("image/png".into()),
    };
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["type"], "url");
    assert_eq!(v["url"], "https://example.com/x.png");
    assert_eq!(v["mimeType"], "image/png");
}

#[test]
fn part_data_carries_structured_json() {
    let p = A2aPart::Data { data: json!({ "k": 1 }) };
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["type"], "data");
    assert_eq!(v["data"]["k"], 1);
}
