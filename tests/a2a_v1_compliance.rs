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

// ---------------------------------------------------------------------------
// TaskStore (Phase 4)
// ---------------------------------------------------------------------------

#[test]
fn task_store_put_get_roundtrip() {
    use rsclaw::a2a::store::TaskStore;
    use rsclaw::a2a::types::{A2aMessage, A2aTask, A2aTaskStatus};
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let store = TaskStore::open(&dir.path().join("a2a.redb")).unwrap();

    let task = A2aTask {
        id: "t-1".into(),
        context_id: Some("ctx".into()),
        status: A2aTaskStatus {
            state: TaskState::Working,
            message: None,
            timestamp: None,
        },
        history: vec![A2aMessage {
            message_id: "m-1".into(),
            role: "ROLE_USER".into(),
            parts: vec![A2aPart::Text { text: "hi".into() }],
            context_id: Some("ctx".into()),
            task_id: Some("t-1".into()),
            metadata: None,
        }],
        artifacts: vec![],
        metadata: None,
    };
    store.put(&task).unwrap();
    let got = store.get("t-1").unwrap().unwrap();
    assert_eq!(got.id, "t-1");
    assert_eq!(got.status.state, TaskState::Working);
    assert_eq!(got.history.len(), 1);

    store.set_status("t-1", TaskState::Completed).unwrap();
    let got = store.get("t-1").unwrap().unwrap();
    assert_eq!(got.status.state, TaskState::Completed);
    assert!(got.status.timestamp.is_some());
}

#[test]
fn task_store_list_pagination() {
    use rsclaw::a2a::store::TaskStore;
    use rsclaw::a2a::types::{A2aTask, A2aTaskStatus};
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let store = TaskStore::open(&dir.path().join("a2a.redb")).unwrap();

    for i in 0..5 {
        store
            .put(&A2aTask {
                id: format!("t-{i:02}"),
                context_id: None,
                status: A2aTaskStatus {
                    state: TaskState::Submitted,
                    message: None,
                    timestamp: None,
                },
                history: vec![],
                artifacts: vec![],
                metadata: None,
            })
            .unwrap();
    }
    let page1 = store.list(0, 2).unwrap();
    assert_eq!(page1.len(), 2);
    let page2 = store.list(2, 2).unwrap();
    assert_eq!(page2.len(), 2);
    let page3 = store.list(4, 2).unwrap();
    assert_eq!(page3.len(), 1);
}

#[test]
fn push_config_crud() {
    use rsclaw::a2a::store::TaskStore;
    use rsclaw::a2a::types::PushNotificationConfig;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let store = TaskStore::open(&dir.path().join("a2a.redb")).unwrap();

    let cfg = PushNotificationConfig {
        id: "p-1".into(),
        task_id: "t-1".into(),
        url: "https://example.com/hook".into(),
        token: "secret".into(),
        authentication: None,
    };
    store.put_push_config(&cfg).unwrap();
    let got = store.get_push_config("t-1", "p-1").unwrap().unwrap();
    assert_eq!(got.url, "https://example.com/hook");
    let listed = store.list_push_configs("t-1").unwrap();
    assert_eq!(listed.len(), 1);
    assert!(store.delete_push_config("t-1", "p-1").unwrap());
    assert!(store.get_push_config("t-1", "p-1").unwrap().is_none());
}

#[test]
fn push_signature_is_stable() {
    use rsclaw::a2a::push::sign_payload;
    // Pre-computed via:
    //   echo -n 'hello' | openssl dgst -sha256 -hmac 'test-secret' -binary | base64
    let sig = sign_payload("test-secret", b"hello");
    assert!(!sig.is_empty(), "signature must be non-empty");
    // Determinism: same inputs → same output.
    assert_eq!(sig, sign_payload("test-secret", b"hello"));
    assert_ne!(sig, sign_payload("test-secret", b"hello!"));
    assert_ne!(sig, sign_payload("other-secret", b"hello"));
}

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

// ---- TurnContext: per-turn observability + control wires ---------------

#[test]
fn turn_context_default_is_no_op() {
    let tc = rsclaw::agent::registry::TurnContext::default();
    assert!(!tc.is_cancelled());
    // No event_tx → emit_working is a silent drop, must not panic.
    tc.emit_working("calling tool memory_search");
}

#[tokio::test]
async fn turn_context_is_cancelled_flips_on_token_fire() {
    let token = tokio_util::sync::CancellationToken::new();
    let tc = rsclaw::agent::registry::TurnContext {
        cancel_token: Some(token.clone()),
        ..Default::default()
    };
    assert!(!tc.is_cancelled());
    token.cancel();
    assert!(tc.is_cancelled());
}

#[tokio::test]
async fn turn_context_emit_working_publishes_status_update() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let tc = rsclaw::agent::registry::TurnContext {
        task_id: Some("t-42".into()),
        context_id: Some("ctx-42".into()),
        event_tx: Some(tx),
        ..Default::default()
    };
    tc.emit_working("calling tool memory_search");

    let ev = rx.recv().await.expect("event");
    match ev {
        rsclaw::a2a::event::AgentEvent::Status {
            task_id, context_id, state, message, final_,
        } => {
            assert_eq!(task_id, "t-42");
            assert_eq!(context_id, "ctx-42");
            assert_eq!(state, rsclaw::a2a::types::TaskState::Working);
            assert!(!final_);
            let msg = message.expect("progress message");
            assert_eq!(msg.role, "agent");
            // First part should be Text with the progress string.
            match &msg.parts[0] {
                rsclaw::a2a::types::A2aPart::Text { text } => {
                    assert_eq!(text, "calling tool memory_search");
                }
                other => panic!("expected Text part, got {other:?}"),
            }
        }
        other => panic!("expected Status event, got {other:?}"),
    }
}

#[tokio::test]
async fn turn_context_request_input_publishes_input_required_and_resumes() {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
    let (ireq_tx, mut ireq_rx) =
        tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<String>>(4);
    let tc = rsclaw::agent::registry::TurnContext {
        task_id: Some("t-42".into()),
        context_id: Some("ctx-42".into()),
        event_tx: Some(event_tx),
        input_request_tx: Some(ireq_tx),
        ..Default::default()
    };

    // Mock the caller: as soon as the runtime registers a resume handle,
    // reply with "from-client" — simulating the next SendMessage carrying
    // the answer.
    let mock = tokio::spawn(async move {
        let resume_tx = ireq_rx.recv().await.expect("resume handle");
        let _ = resume_tx.send("from-client".to_owned());
    });

    let got = tc.request_input("need more info", false).await;
    assert_eq!(got.as_deref(), Some("from-client"));

    // The InputRequired event should have hit event_rx.
    let ev = event_rx.recv().await.expect("event");
    assert!(matches!(
        ev,
        rsclaw::a2a::event::AgentEvent::InputRequired { .. }
    ));

    mock.await.unwrap();
}

// ---- ReplyOutcome wiring ------------------------------------------------

#[test]
fn reply_outcome_default_is_ok() {
    assert_eq!(
        rsclaw::agent::registry::ReplyOutcome::default(),
        rsclaw::agent::registry::ReplyOutcome::Ok
    );
}

#[test]
fn agent_reply_carries_outcome_field() {
    // Build a reply with each variant — purely a struct-shape pin so a
    // future refactor that drops the field fails compilation visibly.
    fn make(outcome: rsclaw::agent::registry::ReplyOutcome) -> rsclaw::agent::AgentReply {
        rsclaw::agent::AgentReply {
            text: "x".into(),
            is_empty: false,
            tool_calls: None,
            images: vec![],
            files: vec![],
            pending_analysis: None,
            needs_outer_done_emit: false,
            outcome,
        }
    }
    assert_eq!(
        make(rsclaw::agent::registry::ReplyOutcome::Error).outcome,
        rsclaw::agent::registry::ReplyOutcome::Error
    );
    assert_eq!(
        make(rsclaw::agent::registry::ReplyOutcome::Canceled).outcome,
        rsclaw::agent::registry::ReplyOutcome::Canceled
    );
}

#[tokio::test]
async fn turn_context_request_input_returns_none_when_resume_tx_dropped() {
    // Mirrors the timeout path: the listener spawn drops the resume_tx
    // (e.g. RSCLAW_A2A_WAIT_INPUT_TIMEOUT_SECS expired before any client
    // resumed). `request_input` must return None so `wait_input` can
    // surface the timeout as a tool error rather than hang forever.
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
    let (ireq_tx, mut ireq_rx) =
        tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<String>>(4);
    let tc = rsclaw::agent::registry::TurnContext {
        task_id: Some("t-42".into()),
        context_id: Some("ctx-42".into()),
        event_tx: Some(event_tx),
        input_request_tx: Some(ireq_tx),
        ..Default::default()
    };

    let mock = tokio::spawn(async move {
        // Receive the resume handle, then deliberately drop it without
        // sending anything — simulates the timeout cleanup tearing the
        // SuspendedTask entry down without a client resume.
        let resume_tx = ireq_rx.recv().await.expect("resume handle");
        drop(resume_tx);
    });

    let got = tc.request_input("need more info", false).await;
    assert!(got.is_none(), "expected None on dropped resume_tx, got {got:?}");
    mock.await.unwrap();
}

#[tokio::test]
async fn turn_context_request_input_with_auth_publishes_auth_required() {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
    let (ireq_tx, mut ireq_rx) =
        tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<String>>(4);
    let tc = rsclaw::agent::registry::TurnContext {
        task_id: Some("t-42".into()),
        context_id: Some("ctx-42".into()),
        event_tx: Some(event_tx),
        input_request_tx: Some(ireq_tx),
        ..Default::default()
    };

    let mock = tokio::spawn(async move {
        let resume_tx = ireq_rx.recv().await.expect("resume handle");
        let _ = resume_tx.send("bearer xyz".to_owned());
    });

    let got = tc.request_input("need bearer token", true).await;
    assert_eq!(got.as_deref(), Some("bearer xyz"));

    let ev = event_rx.recv().await.expect("event");
    assert!(matches!(
        ev,
        rsclaw::a2a::event::AgentEvent::AuthRequired { .. }
    ));

    mock.await.unwrap();
}
