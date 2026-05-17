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

// ---------------------------------------------------------------------------
// AgentEvent wire serialization (Phase 3)
// ---------------------------------------------------------------------------

#[test]
fn agent_event_status_serializes_as_status_update() {
    use rsclaw::a2a::event::AgentEvent;
    let ev = AgentEvent::Status {
        task_id: "t-1".into(),
        context_id: "ctx".into(),
        state: TaskState::Working,
        message: None,
        final_: false,
    };
    let v = ev.to_wire_event();
    assert_eq!(v["kind"], "status-update");
    assert_eq!(v["taskId"], "t-1");
    assert_eq!(v["status"]["state"], "TASK_STATE_WORKING");
    assert_eq!(v["final"], false);
}

#[test]
fn agent_event_artifact_serializes_with_append_flag() {
    use rsclaw::a2a::event::AgentEvent;
    let ev = AgentEvent::Artifact {
        task_id: "t-1".into(),
        context_id: "ctx".into(),
        artifact_id: "a-1".into(),
        parts: vec![A2aPart::Text { text: "chunk".into() }],
        append: true,
        last_chunk: false,
    };
    let v = ev.to_wire_event();
    assert_eq!(v["kind"], "artifact-update");
    assert_eq!(v["artifact"]["artifactId"], "a-1");
    assert_eq!(v["append"], true);
    assert_eq!(v["lastChunk"], false);
}

#[tokio::test]
async fn task_event_bus_fan_out() {
    use rsclaw::a2a::event::{AgentEvent, TaskEventBus};
    let bus = TaskEventBus::new();
    let mut rx1 = bus.subscribe("t-1");
    let mut rx2 = bus.subscribe("t-1");
    let n = bus.publish(AgentEvent::Status {
        task_id: "t-1".into(),
        context_id: "ctx".into(),
        state: TaskState::Working,
        message: None,
        final_: false,
    });
    assert_eq!(n, 2, "should reach both subscribers");
    let e1 = rx1.recv().await.unwrap();
    let e2 = rx2.recv().await.unwrap();
    assert!(matches!(e1, AgentEvent::Status { .. }));
    assert!(matches!(e2, AgentEvent::Status { .. }));
}

#[tokio::test]
async fn subscribe_receives_published_events() {
    use rsclaw::a2a::event::{AgentEvent, TaskEventBus};
    let bus = TaskEventBus::new();
    let mut rx = bus.subscribe("t-2");
    bus.publish(AgentEvent::Status {
        task_id: "t-2".into(),
        context_id: "ctx".into(),
        state: TaskState::Working,
        message: None,
        final_: false,
    });
    let ev = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(ev, AgentEvent::Status { .. }));
}

// ---------------------------------------------------------------------------
// Suspended task registry (Phase 6)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn suspended_task_registry_round_trip() {
    use dashmap::DashMap;
    use rsclaw::a2a::event::SuspendedTask;
    use tokio::sync::oneshot;

    let map: DashMap<String, SuspendedTask> = DashMap::new();
    let (tx, rx) = oneshot::channel();
    map.insert(
        "t-1".into(),
        SuspendedTask {
            task_id: "t-1".into(),
            context_id: "ctx".into(),
            resume_tx: tx,
        },
    );

    let (_, sus) = map.remove("t-1").unwrap();
    let _ = sus.resume_tx.send("my answer".to_owned());
    let got = rx.await.unwrap();
    assert_eq!(got, "my answer");
}
