//! Wire-format types for the Google A2A v1.0 protocol.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::{Uuid, Version};

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
    pub fn err_struct(id: Value, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ---------------------------------------------------------------------------
// Agent Card  (GET /.well-known/agent.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub protocol_version: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,
    #[serde(default)]
    pub version: String,
    pub capabilities: AgentCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_schemes: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<Vec<Value>>,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    pub skills: Vec<AgentSkill>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<AgentExtension>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<AgentCardSignature>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interfaces: Vec<AgentInterface>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    pub streaming: bool,
    pub push_notifications: bool,
    #[serde(default)]
    pub extended_agent_card: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_modes: Vec<String>,
    pub output_modes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProvider {
    pub organization: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentExtension {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCardSignature {
    pub protected: String,
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInterface {
    pub url: String,
    pub transport: String,
}

// ---------------------------------------------------------------------------
// Task (A2A work unit)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2aTask {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub status: A2aTaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<A2aMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<A2aArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2aTaskStatus {
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<A2aMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskState {
    #[serde(rename = "TASK_STATE_UNSPECIFIED")]
    Unspecified,
    #[serde(rename = "TASK_STATE_SUBMITTED")]
    Submitted,
    #[serde(rename = "TASK_STATE_WORKING")]
    Working,
    #[serde(rename = "TASK_STATE_COMPLETED")]
    Completed,
    #[serde(rename = "TASK_STATE_FAILED")]
    Failed,
    #[serde(rename = "TASK_STATE_CANCELED")]
    Canceled,
    #[serde(rename = "TASK_STATE_INPUT_REQUIRED")]
    InputRequired,
    #[serde(rename = "TASK_STATE_AUTH_REQUIRED")]
    AuthRequired,
    #[serde(rename = "TASK_STATE_REJECTED")]
    Rejected,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }

    pub fn is_interrupted(self) -> bool {
        matches!(self, Self::InputRequired | Self::AuthRequired)
    }
}

// ---------------------------------------------------------------------------
// Durable execution identities and records
// ---------------------------------------------------------------------------

/// Error returned when a durable identifier is not a canonical UUID v4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidCanonicalId;

impl fmt::Display for InvalidCanonicalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("durable IDs must be lowercase canonical UUID v4 strings")
    }
}

impl std::error::Error for InvalidCanonicalId {}

macro_rules! durable_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4().to_string())
            }

            pub fn parse(value: impl AsRef<str>) -> Result<Self, InvalidCanonicalId> {
                let value = value.as_ref();
                let parsed = Uuid::parse_str(value).map_err(|_| InvalidCanonicalId)?;
                if parsed.get_version() != Some(Version::Random) || parsed.to_string() != value {
                    return Err(InvalidCanonicalId);
                }
                Ok(Self(value.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = InvalidCanonicalId;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidCanonicalId;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }
    };
}

durable_id!(MachineId);
durable_id!(RepoId);
durable_id!(WorkspaceId);
durable_id!(FleetTeamId);
durable_id!(TaskId);
durable_id!(AttemptId);
durable_id!(WorkId);
durable_id!(AgentId);
durable_id!(OperationId);
durable_id!(FrameId);

/// The persisted delivery classification for an outbound work dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryState {
    NotDelivered,
    DeliveryUnknown,
    Delivered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkState {
    Queued,
    Leased,
    Running,
    WaitingInput,
    Recovering,
    Succeeded,
    Failed,
    Canceled,
    Quarantined,
}

impl WorkState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Canceled | Self::Quarantined
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptState {
    Pending,
    Active,
    Retrying,
    Succeeded,
    Failed,
    Canceled,
}

/// A basic Hub lease. Tokens intentionally do not live in this M1 record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkLease {
    pub lease_epoch: u64,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableTaskRecord {
    pub task_id: TaskId,
    pub fleet_team_id: FleetTeamId,
    pub created_at: String,
    pub current_attempt_id: Option<AttemptId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptRecord {
    pub attempt_id: AttemptId,
    pub task_id: TaskId,
    pub state: AttemptState,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkRecord {
    pub work_id: WorkId,
    pub attempt_id: AttemptId,
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub state: WorkState,
    pub delivery_state: DeliveryState,
    pub lease: WorkLease,
    /// Serialized DispatchWork frame/body, retained before transport insertion.
    pub outbound_dispatch: String,
    /// Serialized Receipt frame/body after authenticated acknowledgement.
    pub receipt: Option<String>,
}

/// The idempotency key required for every durable state mutation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationKey {
    pub actor: String,
    pub kind: String,
    pub operation_id: OperationId,
}

impl OperationKey {
    pub fn storage_key(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}",
            self.actor, self.kind, self.operation_id
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRecord {
    pub key: OperationKey,
    /// Canonical caller-supplied request digest (opaque to the store).
    pub request_digest: String,
    /// Canonical serialized accepted/final result (opaque to the store).
    pub result: String,
}

#[cfg(test)]
mod durable_tests {
    use super::*;

    #[test]
    fn durable_ids_reject_noncanonical_and_non_v4_values() {
        assert!(TaskId::parse("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(TaskId::parse("550E8400-E29B-41D4-A716-446655440000").is_err());
        assert!(TaskId::parse("550e8400-e29b-11d4-a716-446655440000").is_err());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2aMessage {
    pub message_id: String,
    pub role: String,
    pub parts: Vec<A2aPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum A2aPart {
    Text {
        text: String,
    },
    Raw {
        /// Base64-encoded bytes.
        bytes: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Url {
        url: String,
        #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none", default)]
        mime_type: Option<String>,
    },
    Data {
        data: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2aArtifact {
    pub artifact_id: String,
    pub parts: Vec<A2aPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

// ---------------------------------------------------------------------------
// SendMessage / SendStreamingMessage params (v1.0)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageParams {
    pub message: A2aMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

// ---------------------------------------------------------------------------
// Push notification config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushNotificationConfig {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub task_id: String,
    pub url: String,
    /// Shared secret used for HMAC-SHA256 signing of webhook payloads.
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<Value>,
}
