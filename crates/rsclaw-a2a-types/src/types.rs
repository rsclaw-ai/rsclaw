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
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Generates a new canonical UUID v4 identity.
            pub fn new() -> Self {
                Self(Uuid::new_v4().to_string())
            }

            /// Parses a lowercase canonical UUID v4 identity.
            pub fn parse(value: impl AsRef<str>) -> Result<Self, InvalidCanonicalId> {
                let value = value.as_ref();
                let parsed = Uuid::parse_str(value).map_err(|_| InvalidCanonicalId)?;
                if parsed.get_version() != Some(Version::Random) || parsed.to_string() != value {
                    return Err(InvalidCanonicalId);
                }
                Ok(Self(value.to_owned()))
            }

            /// Returns the canonical UUID string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str(&self.0) }
        }

        impl FromStr for $name {
            type Err = InvalidCanonicalId;
            fn from_str(value: &str) -> Result<Self, Self::Err> { Self::parse(value) }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidCanonicalId;
            fn try_from(value: String) -> Result<Self, Self::Error> { Self::parse(value) }
        }
    };
}

durable_id!(
    /// Stable identity assigned to a worker installation.
    MachineId
);
durable_id!(
    /// Stable identity assigned to a repository.
    RepoId
);
durable_id!(
    /// Stable identity assigned to a workspace.
    WorkspaceId
);
durable_id!(
    /// Stable identity of a durable execution fleet.
    FleetTeamId
);
durable_id!(
    /// Stable identity of a durable task.
    TaskId
);
durable_id!(
    /// Stable identity of a task attempt.
    AttemptId
);
durable_id!(
    /// Stable identity of a work dispatch.
    WorkId
);
durable_id!(
    /// Identity of an agent within its assigned machine.
    AgentId
);
durable_id!(
    /// Caller-provided idempotency operation identity.
    OperationId
);
durable_id!(
    /// Stable identity of a relay frame.
    FrameId
);

/// The persisted delivery classification for an outbound work dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryState {
    /// The dispatch has not entered the outbound transport log.
    NotDelivered,
    /// The dispatch entered the log but has no authenticated receipt.
    DeliveryUnknown,
    /// An authenticated matching receipt was persisted.
    Delivered,
}

/// The lifecycle state of one work dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkState {
    /// Work is queued for dispatch.
    Queued,
    /// Work has an active lease.
    Leased,
    /// The worker has started execution.
    Running,
    /// The worker needs external input.
    WaitingInput,
    /// The work is being reconciled.
    Recovering,
    /// The work completed successfully.
    Succeeded,
    /// The work failed.
    Failed,
    /// The work was canceled.
    Canceled,
    /// The work is quarantined.
    Quarantined,
}

impl WorkState {
    /// Returns whether this state cannot transition further.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Canceled | Self::Quarantined
        )
    }
}

/// The lifecycle state of a task attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptState {
    /// The attempt has not started.
    Pending,
    /// The attempt is active.
    Active,
    /// The attempt is awaiting retry.
    Retrying,
    /// The attempt succeeded.
    Succeeded,
    /// The attempt failed.
    Failed,
    /// The attempt was canceled.
    Canceled,
}

/// Opaque lease secret that serializes for transport but is redacted in debug
/// output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeaseToken(String);

impl LeaseToken {
    /// Wraps an already-generated opaque lease token.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns whether the opaque token is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for LeaseToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LeaseToken(REDACTED)")
    }
}

/// A fenced work lease bound to one task, attempt, agent, and machine.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkLease {
    /// Bound task identity.
    pub task_id: TaskId,
    /// Bound attempt identity.
    pub attempt_id: AttemptId,
    /// Bound work identity.
    pub work_id: WorkId,
    /// Assigned agent identity.
    pub agent_id: AgentId,
    /// Machine authorized to use this lease.
    pub assigned_machine_id: MachineId,
    /// Monotonic fencing epoch.
    pub lease_epoch: u64,
    /// RFC 3339 lease expiry.
    pub expires_at: String,
    /// Opaque secret required to exercise the lease.
    pub lease_token: LeaseToken,
}

impl fmt::Debug for WorkLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkLease")
            .field("task_id", &self.task_id)
            .field("attempt_id", &self.attempt_id)
            .field("work_id", &self.work_id)
            .field("agent_id", &self.agent_id)
            .field("assigned_machine_id", &self.assigned_machine_id)
            .field("lease_epoch", &self.lease_epoch)
            .field("expires_at", &self.expires_at)
            .field("lease_token", &"REDACTED")
            .finish()
    }
}

/// Durable task record owned by a fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableTaskRecord {
    pub task_id: TaskId,
    pub fleet_team_id: FleetTeamId,
    pub created_at: String,
    pub current_attempt_id: Option<AttemptId>,
    /// Standard A2A status projected from the authoritative current attempt.
    pub projected_state: TaskState,
}

/// Durable attempt record belonging to one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptRecord {
    pub attempt_id: AttemptId,
    pub task_id: TaskId,
    pub state: AttemptState,
    pub created_at: String,
}

/// Durable work record with its assigned execution resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkRecord {
    pub work_id: WorkId,
    /// Operation that authorized this work dispatch.
    pub operation_id: OperationId,
    pub attempt_id: AttemptId,
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub assigned_machine_id: MachineId,
    pub assigned_repo_id: RepoId,
    pub assigned_workspace_id: WorkspaceId,
    pub state: WorkState,
    pub delivery_state: DeliveryState,
    pub lease: WorkLease,
    /// Serialized DispatchWork frame/body after durable outbound-log insertion.
    pub outbound_dispatch: Option<String>,
    /// Authenticated acknowledgement after exact binding validation.
    pub receipt: Option<WorkReceipt>,
    /// Durable event sequence assigned to the accepted terminal report.
    pub terminal_event_seq: Option<u64>,
    /// Canonical terminal payload used to deduplicate repeated reports.
    pub terminal_payload: Option<String>,
}

/// Typed receipt binding that must exactly match the assigned work lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkReceipt {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub work_id: WorkId,
    pub agent_id: AgentId,
    pub machine_id: MachineId,
    pub lease_epoch: u64,
    /// Serialized authenticated receipt frame/body.
    pub receipt: String,
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
    /// Returns an unambiguous length-prefixed storage key for arbitrary actor
    /// and kind strings.
    pub fn storage_key(&self) -> String {
        format!(
            "{}:{}{}:{}{}",
            self.actor.len(),
            self.actor,
            self.kind.len(),
            self.kind,
            self.operation_id
        )
    }
}

/// Persisted idempotency result explicitly bound to its execution records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRecord {
    pub key: OperationKey,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub work_id: WorkId,
    /// Canonical caller-supplied request digest (opaque to the store).
    pub request_digest: String,
    /// Canonical serialized accepted/final result (opaque to the store).
    pub result: String,
}

/// One immutable, per-task durable event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEvent {
    pub task_id: TaskId,
    pub event_seq: u64,
    /// Opaque canonical serialized event payload.
    pub payload: String,
}

/// Durable event stream bounds for one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEventState {
    pub high_water: u64,
    pub replay_floor: u64,
}

/// A bounded ordered page of replayable task events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEventPage {
    pub events: Vec<TaskEvent>,
    pub high_water: u64,
}

/// Result of requesting events after a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEventReplay {
    /// Events are available in full after the requested cursor.
    Events(TaskEventPage),
    /// The cursor precedes retained history and requires a snapshot resync.
    ResyncRequired { replay_floor: u64, high_water: u64 },
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

    #[test]
    fn generated_durable_ids_are_canonical_v4() {
        let id = WorkId::new();
        assert_eq!(WorkId::parse(id.as_str()), Ok(id));
    }

    #[test]
    fn operation_storage_keys_are_unambiguous() {
        let operation_id = OperationId::new();
        let left = OperationKey {
            actor: "a".to_owned(),
            kind: "bc".to_owned(),
            operation_id: operation_id.clone(),
        };
        let right = OperationKey {
            actor: "ab".to_owned(),
            kind: "c".to_owned(),
            operation_id,
        };
        assert_ne!(left.storage_key(), right.storage_key());
    }

    #[test]
    fn lease_token_debug_is_redacted() {
        let token = LeaseToken::new("secret-lease-token");
        let debug = format!("{token:?}");
        assert!(!token.is_empty());
        assert!(!debug.contains("secret-lease-token"));
        assert!(debug.contains("REDACTED"));
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
