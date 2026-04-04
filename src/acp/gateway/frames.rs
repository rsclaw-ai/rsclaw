//! Gateway Wire Protocol - Frame Types
//!
//! OpenClaw uses a custom frame protocol over WebSocket:
//! - RequestFrame: {"type":"req","id":"...","method":"...","params":{}}
//! - ResponseFrame: {"type":"res","id":"...","ok":true,"payload":{}}
//! - EventFrame: {"type":"event","event":"...","payload":{},"seq":0}
//!
//! Reference:
//! /mnt/j/mickeylan/ai/openclaw/src/gateway/protocol/schema/frames.ts

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Frame Types
// ---------------------------------------------------------------------------

/// Request frame (client → gateway)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestFrame {
    #[serde(rename = "type")]
    pub frame_type: RequestFrameType,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

/// Request frame type marker
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestFrameType {
    Req,
}

/// Response frame (gateway → client)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseFrame {
    #[serde(rename = "type")]
    pub frame_type: ResponseFrameType,
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<ErrorShape>,
}

/// Response frame type marker
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseFrameType {
    Res,
}

/// Event frame (gateway → client, unsolicited)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventFrame {
    #[serde(rename = "type")]
    pub frame_type: EventFrameType,
    pub event: String,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    #[serde(default)]
    pub seq: Option<u64>,
    #[serde(default)]
    pub state_version: Option<StateVersion>,
}

/// Event frame type marker
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventFrameType {
    Event,
}

/// Discriminated union of all frames
/// Uses untagged because each frame already has a `type` discriminator field
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "lowercase")]
pub enum GatewayFrame {
    Req(RequestFrame),
    Res(ResponseFrame),
    Event(EventFrame),
}

// ---------------------------------------------------------------------------
// Error Types
// ---------------------------------------------------------------------------

/// Error shape
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorShape {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Option<serde_json::Value>,
    #[serde(default)]
    pub retryable: Option<bool>,
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
}

/// Error codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    // Auth errors
    AuthRequired,
    AuthInvalid,
    AuthExpired,
    AuthMfaRequired,

    // Client errors (4xx)
    InvalidRequest,
    MethodNotFound,
    InvalidParams,

    // Server errors (5xx)
    InternalError,
    ServerBusy,
    ServerUnavailable,

    // Custom
    RateLimited,
    ProtocolMismatch,
    SessionNotFound,
    SessionBusy,
    AgentNotFound,
    AgentBusy,

    // Catch-all
    Unknown,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::AuthRequired => "auth.required",
            ErrorCode::AuthInvalid => "auth.invalid",
            ErrorCode::AuthExpired => "auth.expired",
            ErrorCode::AuthMfaRequired => "auth.mfa_required",
            ErrorCode::InvalidRequest => "invalid_request",
            ErrorCode::MethodNotFound => "method_not_found",
            ErrorCode::InvalidParams => "invalid_params",
            ErrorCode::InternalError => "internal_error",
            ErrorCode::ServerBusy => "server.busy",
            ErrorCode::ServerUnavailable => "server.unavailable",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::ProtocolMismatch => "protocol.mismatch",
            ErrorCode::SessionNotFound => "session.not_found",
            ErrorCode::SessionBusy => "session.busy",
            ErrorCode::AgentNotFound => "agent.not_found",
            ErrorCode::AgentBusy => "agent.busy",
            ErrorCode::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ---------------------------------------------------------------------------
// State & Snapshot
// ---------------------------------------------------------------------------

/// State version for optimistic concurrency
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateVersion {
    pub version: u64,
    pub updated_at_ms: u64,
}

/// Snapshot of current state
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Snapshot {
    #[serde(default)]
    pub agents: Vec<AgentSummary>,
    #[serde(default)]
    pub sessions: Vec<SessionSummary>,
    #[serde(default)]
    pub channels: Vec<ChannelSummary>,
}

/// Agent summary in snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSummary {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    pub status: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Session summary in snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    pub status: String,
    #[serde(default)]
    pub model: Option<String>,
}

/// Channel summary in snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSummary {
    pub id: String,
    pub kind: String,
    pub status: String,
}

// ---------------------------------------------------------------------------
// Connect Params & Hello
// ---------------------------------------------------------------------------

/// Connect request parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub min_protocol: u32,
    pub max_protocol: u32,
    pub client: ClientInfo,
    #[serde(default)]
    pub caps: Option<Vec<String>>,
    #[serde(default)]
    pub commands: Option<Vec<String>>,
    #[serde(default)]
    pub permissions: Option<std::collections::HashMap<String, bool>>,
    #[serde(default)]
    pub path_env: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    #[serde(default)]
    pub device: Option<DeviceAuth>,
    #[serde(default)]
    pub auth: Option<AuthCredentials>,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
}

/// Client information
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub id: String,
    pub version: String,
    pub platform: String,
    pub mode: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub device_family: Option<String>,
    #[serde(default)]
    pub model_identifier: Option<String>,
    #[serde(default)]
    pub instance_id: Option<String>,
}

/// Device authentication payload
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAuth {
    pub id: String,
    pub public_key: String,
    pub signature: String,
    pub signed_at: u64,
    pub nonce: String,
}

/// Auth credentials
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthCredentials {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub bootstrap_token: Option<String>,
    #[serde(default)]
    pub device_token: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

/// Hello OK response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloOk {
    #[serde(rename = "type")]
    pub frame_type: HelloOkType,
    pub protocol: u32,
    pub server: ServerInfo,
    pub features: Features,
    pub snapshot: Snapshot,
    #[serde(default)]
    pub canvas_host_url: Option<String>,
    #[serde(default)]
    pub auth: Option<IssuedAuth>,
    pub policy: Policy,
}

/// Hello OK type marker
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename = "type")]
pub enum HelloOkType {
    #[serde(rename = "hello-ok")]
    HelloOk,
}

/// Server info in hello
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub version: String,
    pub conn_id: String,
}

/// Supported features
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Features {
    pub methods: Vec<String>,
    pub events: Vec<String>,
}

/// Issued auth token
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuedAuth {
    pub device_token: String,
    pub role: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub issued_at_ms: Option<u64>,
}

/// Server policy
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub max_payload: u64,
    pub max_buffered_bytes: u64,
    pub tick_interval_ms: u64,
}

// ---------------------------------------------------------------------------
// Client IDs & Modes
// ---------------------------------------------------------------------------

/// Client IDs
pub mod client_id {
    pub const GATEWAY_CLIENT: &str = "gateway:client";
    pub const ACP_CLIENT: &str = "acp:client";
    pub const WEBUI_CLIENT: &str = "webui:client";
    pub const CLI_CLIENT: &str = "cli:client";
}

/// Client modes
pub mod client_mode {
    pub const FRONTEND: &str = "frontend";
    pub const BACKEND: &str = "backend";
    pub const CLI: &str = "cli";
    pub const ACP: &str = "acp";
}

/// Default roles
pub mod role {
    pub const OPERATOR: &str = "operator";
    pub const ADMIN: &str = "operator.admin";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_frame_serialization() {
        let frame = RequestFrame {
            frame_type: RequestFrameType::Req,
            id: "req-1".to_string(),
            method: "agent.spawn".to_string(),
            params: Some(serde_json::json!({"cwd": "/tmp"})),
        };

        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""type":"req""#));
        assert!(json.contains(r#""id":"req-1""#));
        assert!(json.contains(r#""method":"agent.spawn""#));

        let parsed: RequestFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "req-1");
        assert_eq!(parsed.method, "agent.spawn");
    }

    #[test]
    fn test_response_frame_serialization() {
        let frame = ResponseFrame {
            frame_type: ResponseFrameType::Res,
            id: "req-1".to_string(),
            ok: true,
            payload: Some(serde_json::json!({"agentId": "a1", "sessionId": "s1"})),
            error: None,
        };

        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""type":"res""#));
        assert!(json.contains(r#""ok":true"#));

        let parsed: ResponseFrame = serde_json::from_str(&json).unwrap();
        assert!(parsed.ok);
    }

    #[test]
    fn test_event_frame_serialization() {
        let frame = EventFrame {
            frame_type: EventFrameType::Event,
            event: "session.message".to_string(),
            payload: Some(serde_json::json!({"content": "hello"})),
            seq: Some(1),
            state_version: None,
        };

        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""type":"event""#));
        assert!(json.contains(r#""event":"session.message""#));
        assert!(json.contains(r#""seq":1"#));

        let parsed: EventFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event, "session.message");
    }

    #[test]
    fn test_gateway_frame_parsing() {
        let json = r#"{"type":"req","id":"1","method":"test","params":{}}"#;
        let frame: GatewayFrame = serde_json::from_str(json).unwrap();
        match frame {
            GatewayFrame::Req(r) => {
                assert_eq!(r.id, "1");
                assert_eq!(r.method, "test");
            }
            _ => panic!("Expected Req frame"),
        }

        let json = r#"{"type":"event","event":"test.event","seq":1}"#;
        let frame: GatewayFrame = serde_json::from_str(json).unwrap();
        match frame {
            GatewayFrame::Event(e) => {
                assert_eq!(e.event, "test.event");
                assert_eq!(e.seq, Some(1));
            }
            _ => panic!("Expected Event frame"),
        }
    }

    #[test]
    fn test_connect_params() {
        let params = ConnectParams {
            min_protocol: 1,
            max_protocol: 10,
            client: ClientInfo {
                id: "rsclaw:client".to_string(),
                version: "0.1.0".to_string(),
                platform: "linux".to_string(),
                mode: "cli".to_string(),
                display_name: Some("rsclaw".to_string()),
                device_family: None,
                model_identifier: None,
                instance_id: None,
            },
            caps: Some(vec!["agent.spawn".to_string()]),
            commands: None,
            permissions: None,
            path_env: None,
            role: Some("operator".to_string()),
            scopes: Some(vec!["operator.admin".to_string()]),
            device: None,
            auth: Some(AuthCredentials {
                token: Some("test-token".to_string()),
                bootstrap_token: None,
                device_token: None,
                password: None,
            }),
            locale: None,
            user_agent: None,
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains(r#""minProtocol":1"#));
        assert!(json.contains(r#""maxProtocol":10"#));
        assert!(json.contains(r#""id":"rsclaw:client""#));
    }
}
