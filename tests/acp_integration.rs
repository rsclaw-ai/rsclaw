//! ACP integration tests

use futures::future::BoxFuture;
use rsclaw::acp::{AcpCallbackHandler, SessionEvent, types::*};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Mock handler for testing
struct MockHandler {
    permissions: Arc<Mutex<Vec<String>>>,
    files_read: Arc<Mutex<Vec<String>>>,
    files_written: Arc<Mutex<Vec<(String, String)>>>,
}

impl MockHandler {
    fn new() -> Self {
        Self {
            permissions: Arc::new(Mutex::new(Vec::new())),
            files_read: Arc::new(Mutex::new(Vec::new())),
            files_written: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl AcpCallbackHandler for MockHandler {
    fn handle_request_permission(
        &self,
        _session_id: &SessionId,
        tool_call_id: &str,
        options: Vec<PermissionOption>,
    ) -> BoxFuture<'_, RequestPermissionOutcome> {
        let permissions = self.permissions.clone();
        let tool_call_id = tool_call_id.to_string();
        Box::pin(async move {
            permissions.lock().await.push(tool_call_id);

            for opt in &options {
                if matches!(opt.kind, PermissionOptionKind::AllowOnce) {
                    return RequestPermissionOutcome::Selected {
                        option_id: opt.option_id.clone(),
                    };
                }
            }
            RequestPermissionOutcome::Cancelled
        })
    }

    fn handle_read_text_file(
        &self,
        _session_id: &SessionId,
        path: &str,
    ) -> BoxFuture<'_, anyhow::Result<String>> {
        let files_read = self.files_read.clone();
        let path = path.to_string();
        Box::pin(async move {
            files_read.lock().await.push(path.clone());
            Ok(format!("mock content for {}", path))
        })
    }

    fn handle_write_text_file(
        &self,
        _session_id: &SessionId,
        path: &str,
        contents: &str,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        let files_written = self.files_written.clone();
        let path = path.to_string();
        let contents = contents.to_string();
        Box::pin(async move {
            files_written.lock().await.push((path, contents));
            Ok(())
        })
    }

    fn handle_terminal_create(
        &self,
        _session_id: &SessionId,
        _command: Option<&str>,
        _args: Option<Vec<String>>,
    ) -> BoxFuture<'_, anyhow::Result<String>> {
        Box::pin(async move { Ok("mock-terminal-1".to_string()) })
    }

    fn handle_terminal_output(
        &self,
        _session_id: &SessionId,
        _terminal_id: &str,
    ) -> BoxFuture<'_, anyhow::Result<TerminalOutputResponse>> {
        Box::pin(async move {
            Ok(TerminalOutputResponse {
                exit: Some(0),
                stdout: "mock output".to_string(),
                stderr: String::new(),
            })
        })
    }

    fn handle_terminal_kill(
        &self,
        _session_id: &SessionId,
        _terminal_id: &str,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn handle_terminal_release(
        &self,
        _session_id: &SessionId,
        _terminal_id: &str,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn handle_terminal_wait_for_exit(
        &self,
        _session_id: &SessionId,
        _terminal_id: &str,
    ) -> BoxFuture<'_, anyhow::Result<Option<i32>>> {
        Box::pin(async move { Ok(Some(0)) })
    }
}

#[tokio::test]
async fn test_types_serialization() {
    let init_req = InitializeRequest {
        protocol_version: 1,
        client_capabilities: ClientCapabilities {
            fs: Some(FileSystemCapabilities {
                read_text_file: true,
                write_text_file: true,
            }),
            terminal: Some(TerminalCapabilities {
                create: true,
                output: true,
                kill: true,
                release: true,
            }),
        },
        client_info: ClientInfo {
            name: "test-client".to_string(),
            version: "1.0.0".to_string(),
        },
    };

    let json = serde_json::to_string(&init_req).unwrap();
    assert!(json.contains("test-client"));
    assert!(json.contains("readTextFile"));

    let parsed: InitializeRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.client_info.name, "test-client");
}

#[tokio::test]
async fn test_content_block_variants() {
    let text_block = ContentBlock::Text {
        text: "Hello world".to_string(),
    };
    let json = serde_json::to_string(&text_block).unwrap();
    assert!(json.contains("text"));

    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    assert!(matches!(parsed, ContentBlock::Text { .. }));
}

#[tokio::test]
async fn test_prompt_request() {
    let req = PromptRequest {
        session_id: "sess_123".to_string(),
        prompt: vec![ContentBlock::Text {
            text: "Hello".to_string(),
        }],
        _meta: None,
    };

    let json = serde_json::to_string(&req).unwrap();
    let parsed: PromptRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.session_id, "sess_123");
}

#[tokio::test]
async fn test_tool_kind_serialization() {
    let kinds = vec![
        ToolKind::Read,
        ToolKind::Edit,
        ToolKind::Delete,
        ToolKind::Move,
        ToolKind::Search,
        ToolKind::Execute,
        ToolKind::Think,
        ToolKind::Fetch,
        ToolKind::Other,
    ];
    for kind in kinds {
        let json = serde_json::to_string(&kind).unwrap();
        let parsed: ToolKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, parsed);
    }
}

#[tokio::test]
async fn test_stop_reason_variants() {
    let reasons = vec![
        StopReason::EndTurn,
        StopReason::MaxTokens,
        StopReason::Cancelled,
        StopReason::Incomplete,
    ];
    for reason in reasons {
        let json = serde_json::to_string(&reason).unwrap();
        let parsed: StopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(reason, parsed);
    }
}

#[tokio::test]
async fn test_permission_option() {
    let opt = PermissionOption {
        option_id: "allow-once".to_string(),
        kind: PermissionOptionKind::AllowOnce,
        label: Some("Allow Once".to_string()),
    };

    let json = serde_json::to_string(&opt).unwrap();
    let parsed: PermissionOption = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.option_id, "allow-once");
    assert!(matches!(parsed.kind, PermissionOptionKind::AllowOnce));
}

#[tokio::test]
async fn test_session_notification_payload() {
    let payload = SessionNotificationPayload::AgentMessageChunk {
        content: TextContent {
            type_: "text".to_string(),
            text: "Hello from agent".to_string(),
        },
    };

    let json = serde_json::to_string(&payload).unwrap();
    assert!(json.contains("agent_message_chunk"));

    let parsed: SessionNotificationPayload = serde_json::from_str(&json).unwrap();
    assert!(matches!(
        parsed,
        SessionNotificationPayload::AgentMessageChunk { .. }
    ));
}

#[tokio::test]
async fn test_terminal_types() {
    let create_req = CreateTerminalRequest {
        session_id: "sess_1".to_string(),
        command: Some("bash".to_string()),
        args: Some(vec!["-c".to_string(), "ls".to_string()]),
    };

    let json = serde_json::to_string(&create_req).unwrap();
    let parsed: CreateTerminalRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.session_id, "sess_1");
    assert_eq!(parsed.command, Some("bash".to_string()));
}

#[tokio::test]
async fn test_file_system_types() {
    let read_req = ReadTextFileRequest {
        path: "/etc/passwd".to_string(),
    };

    let json = serde_json::to_string(&read_req).unwrap();
    let parsed: ReadTextFileRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.path, "/etc/passwd");

    let write_req = WriteTextFileRequest {
        path: "/tmp/test".to_string(),
        contents: "hello".to_string(),
    };

    let json = serde_json::to_string(&write_req).unwrap();
    let parsed: WriteTextFileRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.contents, "hello");
}

#[tokio::test]
async fn test_new_session_response() {
    let json = r#"{"sessionId":"sess_abc123"}"#;
    let resp: NewSessionResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.session_id, "sess_abc123");
}

#[tokio::test]
async fn test_load_session_request() {
    let req = LoadSessionRequest {
        session_id: "sess_xyz".to_string(),
        cwd: Some("/project".to_string()),
        mcp_servers: None,
        _meta: None,
    };

    let json = serde_json::to_string(&req).unwrap();
    let parsed: LoadSessionRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.session_id, "sess_xyz");
}

#[tokio::test]
async fn test_mock_handler_permission() {
    let handler = MockHandler::new();
    let perms = handler.permissions.clone();

    let outcome = handler
        .handle_request_permission(
            &"sess_1".to_string(),
            "call_1",
            vec![
                PermissionOption {
                    option_id: "deny".to_string(),
                    kind: PermissionOptionKind::RejectOnce,
                    label: None,
                },
                PermissionOption {
                    option_id: "allow".to_string(),
                    kind: PermissionOptionKind::AllowOnce,
                    label: None,
                },
            ],
        )
        .await;

    assert!(
        matches!(outcome, RequestPermissionOutcome::Selected { option_id } if option_id == "allow")
    );
    assert_eq!(perms.lock().await.len(), 1);
}

#[tokio::test]
async fn test_mock_handler_file_ops() {
    let handler = MockHandler::new();
    let reads = handler.files_read.clone();
    let writes = handler.files_written.clone();

    let content = handler
        .handle_read_text_file(&"sess_1".to_string(), "/test/path")
        .await
        .unwrap();
    assert_eq!(content, "mock content for /test/path");
    assert_eq!(reads.lock().await.len(), 1);

    handler
        .handle_write_text_file(&"sess_1".to_string(), "/test/out", "data")
        .await
        .unwrap();
    let w = writes.lock().await;
    assert_eq!(w.len(), 1);
    assert_eq!(w[0], ("/test/out".to_string(), "data".to_string()));
}

#[tokio::test]
async fn test_session_event_variants() {
    let events = vec![
        SessionEvent::AgentMessageChunk {
            content: "test".to_string(),
        },
        SessionEvent::AgentThoughtChunk {
            content: "thinking".to_string(),
        },
        SessionEvent::ToolCallStarted {
            tool_call_id: "call_1".to_string(),
            title: Some("Read file".to_string()),
            kind: ToolKind::Read,
        },
        SessionEvent::ToolCallInProgress {
            tool_call_id: "call_1".to_string(),
        },
        SessionEvent::ToolCallCompleted {
            tool_call_id: "call_1".to_string(),
            result: Some("done".to_string()),
        },
        SessionEvent::ToolCallFailed {
            tool_call_id: "call_1".to_string(),
            error: "oops".to_string(),
        },
        SessionEvent::ModeChanged {
            mode_id: "code".to_string(),
        },
        SessionEvent::UsageUpdated {
            used: 100,
            size: 1000,
        },
    ];

    for event in events {
        match event {
            SessionEvent::AgentMessageChunk { content } => assert_eq!(content, "test"),
            SessionEvent::ToolCallStarted { kind, .. } => assert_eq!(kind, ToolKind::Read),
            SessionEvent::UsageUpdated { used, size } => {
                assert_eq!(used, 100);
                assert_eq!(size, 1000);
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn test_list_sessions_response() {
    let json = r#"{"sessions":[{"sessionId":"s1","cwd":"/p1","title":"Session 1"}]}"#;
    let resp: ListSessionsResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.sessions.len(), 1);
    assert_eq!(resp.sessions[0].session_id, "s1");
    assert_eq!(resp.sessions[0].cwd, "/p1");
}

#[tokio::test]
async fn test_session_config_option() {
    let opt = SessionConfigOption {
        type_: "string".to_string(),
        id: "model".to_string(),
        name: "Model".to_string(),
        description: Some("LLM model to use".to_string()),
        category: Some("generation".to_string()),
        current_value: "gpt-4".to_string(),
        options: Some(vec![
            serde_json::json!("gpt-4"),
            serde_json::json!("claude-3"),
        ]),
    };

    let json = serde_json::to_string(&opt).unwrap();
    let parsed: SessionConfigOption = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.id, "model");
    assert_eq!(parsed.current_value, "gpt-4");
}
