//! Durable relay wire contract shared by Hub and spokes.
//!
//! This deliberately has no compatibility decoding for the former relay frame.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{
    AgentId, AttemptId, FleetTeamId, FrameId, MachineId, OperationId, RepoId, TaskId, WorkId,
    WorkspaceId,
};

pub const RELAY_PROTOCOL: &str = "rsclaw.a2a.relay.v1";
pub const MAX_ROUTE_ENTRIES: usize = 8;
pub const MAX_HOPS: u64 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteEntry {
    pub machine_id: MachineId,
    pub route_epoch: u64,
}

#[derive(Clone, PartialEq)]
pub struct RelayFrame {
    pub frame_id: FrameId,
    pub fleet_team_id: FleetTeamId,
    pub machine_id: MachineId,
    pub sent_at: String,
    pub kind: RelayKind,
    pub seq: u64,
    pub ack: u64,
    pub cursor: u64,
    pub route: Vec<RouteEntry>,
    pub body: RelayBody,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RelayFrameWire {
    frame_id: FrameId,
    fleet_team_id: FleetTeamId,
    machine_id: MachineId,
    sent_at: String,
    kind: RelayKind,
    seq: u64,
    ack: u64,
    cursor: u64,
    route: Vec<RouteEntry>,
    body: Value,
}

impl Serialize for RelayFrame {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let body = self.body.to_value().map_err(serde::ser::Error::custom)?;
        RelayFrameWire {
            frame_id: self.frame_id.clone(),
            fleet_team_id: self.fleet_team_id.clone(),
            machine_id: self.machine_id.clone(),
            sent_at: self.sent_at.clone(),
            kind: self.kind,
            seq: self.seq,
            ack: self.ack,
            cursor: self.cursor,
            route: self.route.clone(),
            body,
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for RelayFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = RelayFrameWire::deserialize(deserializer)?;
        let body = RelayBody::from_value(wire.kind, wire.body).map_err(serde::de::Error::custom)?;
        let frame = Self {
            frame_id: wire.frame_id,
            fleet_team_id: wire.fleet_team_id,
            machine_id: wire.machine_id,
            sent_at: wire.sent_at,
            kind: wire.kind,
            seq: wire.seq,
            ack: wire.ack,
            cursor: wire.cursor,
            route: wire.route,
            body,
        };
        frame.validate().map_err(serde::de::Error::custom)?;
        Ok(frame)
    }
}

impl RelayFrame {
    /// Validates envelope bounds and the body selected by `kind`. Call this
    /// after deserialization and before route registration or execution.
    pub fn validate(&self) -> Result<(), RelayFrameError> {
        if !valid_timestamp(&self.sent_at) {
            return Err(RelayFrameError::InvalidTimestamp);
        }
        if self.seq == 0 {
            return Err(RelayFrameError::InvalidSequence);
        }
        if self.route.len() > MAX_ROUTE_ENTRIES {
            return Err(RelayFrameError::RouteTooLong);
        }
        if self.route.iter().any(|entry| entry.route_epoch == 0) {
            return Err(RelayFrameError::InvalidBody);
        }
        let mut seen = std::collections::HashSet::new();
        if self
            .route
            .iter()
            .any(|entry| !seen.insert(&entry.machine_id))
        {
            return Err(RelayFrameError::RouteLoop);
        }
        self.body.validate_for(self.kind)
    }

    /// Appends one forwarding entry while enforcing the durable route bound.
    pub fn forwarded(
        mut self,
        machine_id: MachineId,
        route_epoch: u64,
    ) -> Result<Self, RelayFrameError> {
        self.validate()?;
        if self
            .route
            .iter()
            .any(|entry| entry.machine_id == machine_id)
        {
            return Err(RelayFrameError::RouteLoop);
        }
        if self.route.len() == MAX_ROUTE_ENTRIES {
            return Err(RelayFrameError::RouteTooLong);
        }
        match &mut self.body {
            RelayBody::DispatchWork(body) => decrement_hops(&mut body.hops_remaining)?,
            RelayBody::Wake(body) => decrement_hops(&mut body.hops_remaining)?,
            _ => {}
        }
        self.route.push(RouteEntry {
            machine_id,
            route_epoch,
        });
        Ok(self)
    }
}

fn valid_timestamp(timestamp: &str) -> bool {
    let bytes = timestamp.as_bytes();
    if bytes.len() != 24
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || bytes.get(19) != Some(&b'.')
        || bytes.get(23) != Some(&b'Z')
        || !bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        })
    {
        return false;
    }

    let parse = |range: std::ops::Range<usize>| {
        std::str::from_utf8(&bytes[range])
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
    };
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        parse(0..4),
        parse(5..7),
        parse(8..10),
        parse(11..13),
        parse(14..16),
        parse(17..19),
    ) else {
        return false;
    };
    if year == 0 || !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return false;
    }
    let leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_month = match month {
        2 if leap_year => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=days_in_month).contains(&day)
}

fn decrement_hops(hops: &mut u64) -> Result<(), RelayFrameError> {
    if *hops == 0 || *hops > MAX_HOPS {
        return Err(RelayFrameError::HopsExhausted);
    }
    *hops -= 1;
    Ok(())
}

fn valid_lease(lease: &LeaseBody) -> bool {
    lease.lease_epoch > 0
        && valid_timestamp(&lease.expires_at)
        && !lease.lease_token.is_empty()
        && lease.lease_token.len() <= 1024
}

fn valid_worker_binding(binding: &WorkerBinding) -> bool {
    binding.lease_epoch > 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayKind {
    Hello,
    HelloAck,
    RegisterAgent,
    Reconcile,
    DispatchWork,
    Receipt,
    WorkStarted,
    WorkProgress,
    WorkTerminal,
    RenewLease,
    LeaseGranted,
    LeaseRejected,
    Event,
    EventAck,
    ReplayRequest,
    ReplayEvent,
    ResyncRequired,
    CancelWork,
    Quarantine,
    Wake,
    Ping,
    Pong,
    Error,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_durableRelayBody", rename_all = "camelCase")]
pub enum RelayBody {
    Hello(HelloBody),
    HelloAck(HelloAckBody),
    DispatchWork(DispatchWorkBody),
    Receipt(ReceiptBody),
    WorkStarted(WorkerBinding),
    WorkProgress(WorkProgressBody),
    WorkTerminal(WorkTerminalBody),
    Event(EventBody),
    ReplayEvent(EventBody),
    EventAck(EventAckBody),
    ReplayRequest(ReplayRequestBody),
    ResyncRequired(ResyncRequiredBody),
    Wake(WakeBody),
    Ping(NonceBody),
    Pong(NonceBody),
    /// Known but not yet execution-bearing control kinds. They are retained as
    /// JSON only so an M1 peer can reject malformed execution frames
    /// fail-closed.
    Control(Value),
}

impl RelayBody {
    fn to_value(&self) -> Result<Value, serde_json::Error> {
        match self {
            Self::Hello(v) => serde_json::to_value(v),
            Self::HelloAck(v) => serde_json::to_value(v),
            Self::DispatchWork(v) => serde_json::to_value(v),
            Self::Receipt(v) => serde_json::to_value(v),
            Self::WorkStarted(v) => serde_json::to_value(v),
            Self::WorkProgress(v) => serde_json::to_value(v),
            Self::WorkTerminal(v) => serde_json::to_value(v),
            Self::Event(v) | Self::ReplayEvent(v) => serde_json::to_value(v),
            Self::EventAck(v) => serde_json::to_value(v),
            Self::ReplayRequest(v) => serde_json::to_value(v),
            Self::ResyncRequired(v) => serde_json::to_value(v),
            Self::Wake(v) => serde_json::to_value(v),
            Self::Ping(v) | Self::Pong(v) => serde_json::to_value(v),
            Self::Control(v) => Ok(v.clone()),
        }
    }
    fn from_value(kind: RelayKind, value: Value) -> Result<Self, serde_json::Error> {
        Ok(match kind {
            RelayKind::Hello => Self::Hello(serde_json::from_value(value)?),
            RelayKind::HelloAck => Self::HelloAck(serde_json::from_value(value)?),
            RelayKind::DispatchWork => Self::DispatchWork(serde_json::from_value(value)?),
            RelayKind::Receipt => Self::Receipt(serde_json::from_value(value)?),
            RelayKind::WorkStarted => Self::WorkStarted(serde_json::from_value(value)?),
            RelayKind::WorkProgress => Self::WorkProgress(serde_json::from_value(value)?),
            RelayKind::WorkTerminal => Self::WorkTerminal(serde_json::from_value(value)?),
            RelayKind::Event => Self::Event(serde_json::from_value(value)?),
            RelayKind::ReplayEvent => Self::ReplayEvent(serde_json::from_value(value)?),
            RelayKind::EventAck => Self::EventAck(serde_json::from_value(value)?),
            RelayKind::ReplayRequest => Self::ReplayRequest(serde_json::from_value(value)?),
            RelayKind::ResyncRequired => Self::ResyncRequired(serde_json::from_value(value)?),
            RelayKind::Wake => Self::Wake(serde_json::from_value(value)?),
            RelayKind::Ping => Self::Ping(serde_json::from_value(value)?),
            RelayKind::Pong => Self::Pong(serde_json::from_value(value)?),
            RelayKind::RegisterAgent
            | RelayKind::Reconcile
            | RelayKind::RenewLease
            | RelayKind::LeaseGranted
            | RelayKind::LeaseRejected
            | RelayKind::CancelWork
            | RelayKind::Quarantine
            | RelayKind::Error => Self::Control(value),
        })
    }
    fn validate_for(&self, kind: RelayKind) -> Result<(), RelayFrameError> {
        let matches = matches!(
            (kind, self),
            (RelayKind::Hello, Self::Hello(_))
                | (RelayKind::HelloAck, Self::HelloAck(_))
                | (RelayKind::DispatchWork, Self::DispatchWork(_))
                | (RelayKind::Receipt, Self::Receipt(_))
                | (RelayKind::WorkStarted, Self::WorkStarted(_))
                | (RelayKind::WorkProgress, Self::WorkProgress(_))
                | (RelayKind::WorkTerminal, Self::WorkTerminal(_))
                | (RelayKind::Event, Self::Event(_))
                | (RelayKind::ReplayEvent, Self::ReplayEvent(_))
                | (RelayKind::EventAck, Self::EventAck(_))
                | (RelayKind::ReplayRequest, Self::ReplayRequest(_))
                | (RelayKind::ResyncRequired, Self::ResyncRequired(_))
                | (RelayKind::Wake, Self::Wake(_))
                | (RelayKind::Ping, Self::Ping(_))
                | (RelayKind::Pong, Self::Pong(_))
                | (
                    RelayKind::RegisterAgent
                        | RelayKind::Reconcile
                        | RelayKind::RenewLease
                        | RelayKind::LeaseGranted
                        | RelayKind::LeaseRejected
                        | RelayKind::CancelWork
                        | RelayKind::Quarantine
                        | RelayKind::Error,
                    Self::Control(_)
                )
        );
        if !matches {
            return Err(RelayFrameError::BodyDoesNotMatchKind);
        }
        match self {
            Self::Hello(body) if body.protocol != RELAY_PROTOCOL => Err(RelayFrameError::Protocol),
            Self::Hello(body) if body.authority_epoch == 0 || !body.auth.is_object() => {
                Err(RelayFrameError::InvalidBody)
            }
            Self::HelloAck(body) if body.authority_epoch == 0 || body.connection_id.is_empty() => {
                Err(RelayFrameError::InvalidBody)
            }
            Self::DispatchWork(body) if body.hops_remaining > MAX_HOPS => {
                Err(RelayFrameError::HopsExhausted)
            }
            Self::DispatchWork(body) if !valid_lease(&body.lease) => {
                Err(RelayFrameError::InvalidBody)
            }
            Self::Receipt(body) if body.lease_epoch == 0 => Err(RelayFrameError::InvalidBody),
            Self::WorkStarted(body) if !valid_worker_binding(body) => {
                Err(RelayFrameError::InvalidBody)
            }
            Self::WorkProgress(body) if !valid_worker_binding(&body.binding) => {
                Err(RelayFrameError::InvalidBody)
            }
            Self::WorkTerminal(body)
                if !valid_worker_binding(&body.binding)
                    || !matches!(body.outcome.as_str(), "Succeeded" | "Failed" | "Canceled")
                    || body.result.is_some() == body.failure.is_some() =>
            {
                Err(RelayFrameError::InvalidBody)
            }
            Self::Event(body) | Self::ReplayEvent(body) if body.event_seq == 0 => {
                Err(RelayFrameError::InvalidBody)
            }
            Self::ReplayRequest(body) if body.limit == 0 || body.limit > 1000 => {
                Err(RelayFrameError::InvalidBody)
            }
            Self::Wake(body) if body.hops_remaining > MAX_HOPS => {
                Err(RelayFrameError::HopsExhausted)
            }
            Self::Ping(body) | Self::Pong(body)
                if body.nonce.is_empty() || body.nonce.len() > 256 =>
            {
                Err(RelayFrameError::InvalidBody)
            }
            Self::Control(body) if !body.is_object() => Err(RelayFrameError::InvalidBody),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HelloBody {
    pub protocol: String,
    pub authority_epoch: u64,
    pub auth: Value,
    pub dev_mode: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HelloAckBody {
    pub authority_epoch: u64,
    pub connection_id: String,
    pub limits: Value,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseBody {
    pub lease_epoch: u64,
    pub expires_at: String,
    pub lease_token: String,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DispatchWorkBody {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub work_id: WorkId,
    pub agent_id: AgentId,
    pub repo_id: RepoId,
    pub workspace_id: WorkspaceId,
    pub lease: LeaseBody,
    pub operation_id: OperationId,
    pub hops_remaining: u64,
    pub payload: Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptBody {
    pub work_id: WorkId,
    pub attempt_id: AttemptId,
    pub agent_id: AgentId,
    pub lease_epoch: u64,
    pub frame_id: FrameId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerBinding {
    pub work_id: WorkId,
    pub attempt_id: AttemptId,
    pub agent_id: AgentId,
    pub lease_epoch: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkProgressBody {
    #[serde(flatten)]
    pub binding: WorkerBinding,
    pub progress: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkTerminalBody {
    #[serde(flatten)]
    pub binding: WorkerBinding,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventBody {
    pub event_seq: u64,
    pub task_id: TaskId,
    pub event: Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventAckBody {
    pub event_seq: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplayRequestBody {
    pub after_event_seq: u64,
    pub limit: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResyncRequiredBody {
    pub replay_floor: u64,
    pub snapshot: Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WakeBody {
    pub work_id: WorkId,
    pub wake_generation: u64,
    pub reason: String,
    pub hops_remaining: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NonceBody {
    pub nonce: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayFrameError {
    InvalidTimestamp,
    InvalidSequence,
    RouteTooLong,
    RouteLoop,
    HopsExhausted,
    BodyDoesNotMatchKind,
    InvalidBody,
    Protocol,
}

impl std::fmt::Display for RelayFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidTimestamp => "relay sentAt is not RFC 3339 UTC",
            Self::InvalidSequence => "relay seq must begin at one",
            Self::RouteTooLong => "relay route has too many entries",
            Self::RouteLoop => "relay route contains a loop",
            Self::HopsExhausted => "relay hops exhausted or invalid",
            Self::BodyDoesNotMatchKind => "relay body does not match kind",
            Self::InvalidBody => "relay body contains invalid values",
            Self::Protocol => "unsupported relay protocol",
        };
        f.write_str(message)
    }
}
impl std::error::Error for RelayFrameError {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_frame_shape_is_not_a_durable_frame() {
        assert!(serde_json::from_str::<RelayFrame>(r#"{"type":"ping","ts":1}"#).is_err());
    }
    #[test]
    fn serialization_has_the_documented_unwrapped_body() {
        let frame = RelayFrame {
            frame_id: FrameId::new(),
            fleet_team_id: FleetTeamId::new(),
            machine_id: MachineId::new(),
            sent_at: "2026-09-02T07:28:00.000Z".into(),
            kind: RelayKind::Ping,
            seq: 1,
            ack: 0,
            cursor: 0,
            route: vec![],
            body: RelayBody::Ping(NonceBody { nonce: "n".into() }),
        };
        let json = serde_json::to_value(frame).unwrap();
        assert_eq!(json["body"]["nonce"], "n");
        assert!(json["body"].get("_durableRelayBody").is_none());
    }

    #[test]
    fn invalid_calendar_timestamp_is_rejected() {
        let json = serde_json::json!({
            "frameId": FrameId::new(),
            "fleetTeamId": FleetTeamId::new(),
            "machineId": MachineId::new(),
            "sentAt": "2026-02-30T25:61:61.000Z",
            "kind": "Ping",
            "seq": 1,
            "ack": 0,
            "cursor": 0,
            "route": [],
            "body": { "nonce": "n" }
        });
        assert!(serde_json::from_value::<RelayFrame>(json).is_err());
    }

    #[test]
    fn route_forwarding_is_bounded_and_loop_free() {
        let id = MachineId::new();
        let frame = RelayFrame {
            frame_id: FrameId::new(),
            fleet_team_id: FleetTeamId::new(),
            machine_id: id.clone(),
            sent_at: "2026-09-02T07:28:00.000Z".into(),
            kind: RelayKind::Ping,
            seq: 1,
            ack: 0,
            cursor: 0,
            route: vec![RouteEntry {
                machine_id: id.clone(),
                route_epoch: 1,
            }],
            body: RelayBody::Ping(NonceBody { nonce: "n".into() }),
        };
        assert!(matches!(
            frame.forwarded(id, 1),
            Err(RelayFrameError::RouteLoop)
        ));
    }
}
