//! Wire frame types for the OpenClaw WebSocket Gateway Protocol v3.
//!
//! Three frame kinds:
//!   - `ReqFrame`   (client -> server)
//!   - `ResFrame`   (server -> client)
//!   - `EventFrame` (server -> client, push)

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 3;

// ---------------------------------------------------------------------------
// Inbound (client -> server)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InboundFrame {
    Req(ReqFrame),
}

#[derive(Debug, Deserialize)]
pub struct ReqFrame {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Outbound: ResFrame
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ResFrame {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorShape>,
}

impl ResFrame {
    pub fn ok(id: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            kind: "res",
            id: id.into(),
            ok: true,
            payload: Some(payload),
            error: None,
        }
    }

    pub fn err(id: impl Into<String>, error: ErrorShape) -> Self {
        Self {
            kind: "res",
            id: id.into(),
            ok: false,
            payload: None,
            error: Some(error),
        }
    }
}

// ---------------------------------------------------------------------------
// Outbound: EventFrame
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventFrame {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub event: String,
    pub payload: serde_json::Value,
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_version: Option<u64>,
}

impl EventFrame {
    pub fn new(event: impl Into<String>, payload: serde_json::Value, seq: u64) -> Self {
        Self {
            kind: "event",
            event: event.into(),
            payload,
            seq,
            state_version: None,
        }
    }
}

// ---------------------------------------------------------------------------
// ErrorShape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorShape {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub retryable: bool,
    pub retry_after_ms: u64,
}

impl ErrorShape {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "internal_error".to_owned(),
            message: message.into(),
            details: None,
            retryable: false,
            retry_after_ms: 0,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: "not_found".to_owned(),
            message: message.into(),
            details: None,
            retryable: false,
            retry_after_ms: 0,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            code: "bad_request".to_owned(),
            message: message.into(),
            details: None,
            retryable: false,
            retry_after_ms: 0,
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            code: "unauthorized".to_owned(),
            message: message.into(),
            details: None,
            retryable: false,
            retry_after_ms: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Handshake payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub min_protocol: Option<u32>,
    pub max_protocol: Option<u32>,
    pub auth: Option<AuthCredentials>,
    pub device_id: Option<String>,
    pub role: Option<String>,
    pub client: Option<ClientInfo>,
}

/// Metadata about the connecting client (sent in the connect handshake).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub id: Option<String>,
    pub version: Option<String>,
    pub platform: Option<String>,
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthCredentials {
    pub token: Option<String>,
    pub device_token: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRecord {
    pub device_token: String,
    pub device_id: Option<String>,
    pub created_at: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloOkPayload {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub protocol: u32,
    pub server: ServerInfo,
    pub features: FeaturesInfo,
    pub auth: HelloAuth,
    pub policy: PolicyInfo,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub agent_count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeaturesInfo {
    pub streaming: bool,
    pub multi_agent: bool,
    pub memory: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloAuth {
    pub device_token: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyInfo {
    pub max_message_length: u64,
    pub rate_limit_rpm: u64,
    pub tick_interval_ms: u64,
}
