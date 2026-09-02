# Durable A2A execution interface (durable RelayFrame schema)

**Status:** authoritative contract for ADR 0003. This replaces the RelayFrame
schema in place while retaining the existing Hello `protocol` discriminator:
`rsclaw.a2a.relay.v1`. That string identifies this relay family, not a compatible
frame layout. An old-schema frame, a version-negotiation request, or a mixed
old-schema/durable-schema connection is a protocol error; no compatibility behavior
exists.

## Normative conventions

**Milestone 1 ships:** a single authoritative Hub with local redb, stable fleet/
machine/repository/workspace identities, Task/Attempt/Work records, idempotent
Create/Resume/Cancel, persisted dispatch/receipt delivery classification, a basic
lease with stale-epoch rejection, and durable A2A task snapshots. The Hub and all
workers cut over atomically to this schema. **Later stages:** event replay and
compaction; full renew/fencing/reconciliation, restart recovery and quarantine;
machine-key authentication plus restricted dev mode; then passive-standby promotion.
M1 does not ship those later-stage capabilities.

All IDs are non-empty, ASCII, canonical UUIDv7 or ULID strings (one format is
selected by implementation configuration and remains stable per fleet). Timestamps
are RFC 3339 UTC with milliseconds. Integers are unsigned. JSON fields use
camelCase. A receiver rejects unknown `kind`, a missing required field, an invalid
ID, duplicate JSON keys, a non-object top level, or a body not valid for its kind.
Optional fields may be absent but not `null` unless explicitly stated. Secrets,
lease tokens, private keys, and decrypted payloads must not be logged.

`TaskId -> AttemptId -> WorkId -> AgentId` means: a task owns zero or more
attempts; an attempt owns one or more work dispatches over time; a work has exactly
one assigned agent; AgentId is stable only within its registered MachineId and
agent generation. The Hub's redb records are authoritative. A connection ID,
route entry, A2A `contextId`, and A2A `agentId` are never substitutes for these
IDs.

## Rust contract

```rust
/// Durable Hub authority. Exactly one authority epoch may accept mutations for a
/// FleetTeamId; this trait is not an active-active replication interface.
pub trait DurableExecutionHub: Send + Sync {
    /// Atomically deduplicates an operation and either returns its former result
    /// or commits its new result plus any generated task event.
    async fn apply(&self, command: DurableCommand) -> Result<OperationResult, DurableError>;

    /// Returns a durable task projection and its replay cursor to an authorized caller.
    async fn task(&self, task_id: TaskId, actor: Actor) -> Result<TaskSnapshot, DurableError>;

    /// Returns ordered events strictly after `after`; a compacted cursor yields
    /// ResyncRequired rather than a partial history.
    async fn replay(&self, after: EventSeq, limit: u32, actor: Actor)
        -> Result<Replay, DurableError>;

    /// Registers/reconciles one authenticated worker and its durable local ledger.
    async fn reconcile_worker(&self, request: WorkerReconcile)
        -> Result<WorkerReconcileResult, DurableError>;
}

/// A worker accepts only a currently fenced lease; it never commits Task state itself.
pub trait DurableExecutionWorker: Send + Sync {
    /// Handles a Hub frame exactly once for `(frame_id, lease_epoch)` and returns
    /// a receipt or protocol error. Duplicate accepted frames are ACKed, not rerun.
    async fn handle(&self, frame: RelayFrame) -> Result<WorkerReply, DurableError>;
}

pub type MachineId = CanonicalId;
pub type RepoId = CanonicalId;
pub type WorkspaceId = CanonicalId;
pub type FleetTeamId = CanonicalId;
pub type TaskId = CanonicalId;
pub type AttemptId = CanonicalId;
pub type WorkId = CanonicalId;
pub type AgentId = CanonicalId;
pub type OperationId = CanonicalId;
pub type FrameId = CanonicalId;
pub type EventSeq = u64;
pub type LeaseEpoch = u64;
pub type AuthorityEpoch = u64;

pub struct RelayFrame {
    pub frame_id: FrameId,
    pub fleet_team_id: FleetTeamId,
    pub machine_id: MachineId,       // must equal authenticated peer identity
    pub sent_at: Timestamp,
    pub kind: RelayKind,
    pub seq: u64,                    // sender connection sequence, begins at 1
    pub ack: u64,                    // greatest contiguous peer seq received, 0 if none
    pub cursor: EventSeq,            // greatest Hub event processed, 0 if none
    pub route: RouteTrace,
    pub body: RelayBody,
}

pub enum RelayKind {
    Hello, HelloAck, RegisterAgent, Reconcile, DispatchWork, Receipt, WorkStarted,
    WorkProgress, WorkTerminal, RenewLease, LeaseGranted, LeaseRejected,
    Event, EventAck, ReplayRequest, ReplayEvent, ResyncRequired, CancelWork,
    Quarantine, Wake, Ping, Pong, Error,
}

pub enum DeliveryState { NotDelivered, DeliveryUnknown, Delivered }
pub enum WorkState { Queued, Leased, Running, WaitingInput, Recovering, Succeeded, Failed, Canceled, Quarantined }
pub enum AttemptState { Pending, Active, Retrying, Succeeded, Failed, Canceled }

pub struct WorkLease {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub work_id: WorkId,
    pub agent_id: AgentId,
    pub assigned_machine_id: MachineId,
    pub lease_epoch: LeaseEpoch,
    pub expires_at: Timestamp,
    pub lease_token: SecretBytes,
}
```

`DurableCommand` includes `CreateTask`, `ResumeTask`, `CancelTask`,
`RetryAttempt`, `AcknowledgeEvent`, `QuarantineWorker`, `ClearQuarantine`, and
`PromoteStandby`. Every mutating variant contains `actor`, `operationId`, and a
canonical request digest. `PromoteStandby` is manual and must fence the prior
authority epoch; it is not automatic failover.

## Durable RelayFrame JSON format

Every frame has exactly these envelope fields and body fields are selected by
`kind`. The required `Hello.body.protocol` is exactly `rsclaw.a2a.relay.v1`,
retained in its existing discriminator location; it does not select the old schema.
`seq`, `ack`, and `cursor` are required even on Ping/Pong. `route` is required and
may be `[]` only before the first Hub forward.

```json
{
  "frameId": "018f...",
  "fleetTeamId": "018f...",
  "machineId": "018f...",
  "sentAt": "2026-09-02T07:28:00.000Z",
  "kind": "DispatchWork",
  "seq": 41,
  "ack": 39,
  "cursor": 918,
  "route": [{ "machineId": "018f...", "routeEpoch": 7 }],
  "body": {
    "taskId": "018f...",
    "attemptId": "018f...",
    "workId": "018f...",
    "agentId": "018f...",
    "repoId": "018f...",
    "workspaceId": "018f...",
    "lease": {
      "leaseEpoch": 12,
      "expiresAt": "2026-09-02T07:29:00.000Z",
      "leaseToken": "opaque-secret"
    },
    "operationId": "018f...",
    "hopsRemaining": 4,
    "payload": { "encoding": "hub-visible-json", "value": {} }
  }
}
```

`route` entries are `{ machineId, routeEpoch }`, unique within a frame, maximum
8. `routeEpoch` is the forwarding authority/route epoch, not `leaseEpoch`. The
Hub initializes or validates the trace; a forward decrements `body.hopsRemaining`
(where present; initial value is 4), appends its entry, and rejects a repeated
MachineId, exhausted hop count, oversized trace, or a wake repeat. A receiver
never trusts a sender-provided route to authorize dispatch.

### Bodies and required fields

| Kind | Required `body` fields | State effect |
|---|---|---|
| `Hello` | `protocol` = `rsclaw.a2a.relay.v1`, `authorityEpoch`, `auth`, `devMode` | authenticate only |
| `HelloAck` | `authorityEpoch`, `connectionId`, `limits` | establishes durable-schema session |
| `RegisterAgent` | `agentId`, `agentGeneration`, `repoId`, `workspaceId`, `capabilities` | registration, no lease |
| `Reconcile` | `workerLedger[]` with `workId`, `leaseEpoch`, `localState` | restart reconciliation |
| `DispatchWork` | task/attempt/work/agent IDs, repo/workspace IDs, `lease`, `operationId`, `payload`, `hopsRemaining` | accepted outbound record; initially NotDelivered |
| `Receipt` | `workId`, `attemptId`, `agentId`, `leaseEpoch`, `frameId` | marks delivery Delivered |
| `WorkStarted` | `workId`, `attemptId`, `agentId`, `leaseEpoch` | Leased -> Running |
| `WorkProgress` | same work tuple, `progress`, optional `artifact` | appends event only |
| `WorkTerminal` | same work tuple, `outcome`, `result` or `failure` | fenced terminal transition |
| `RenewLease` | same work tuple, `leaseToken`, `requestedUntil` | idempotent renew request |
| `LeaseGranted` | same work tuple, `leaseEpoch`, `expiresAt`, `leaseToken` | extends current lease |
| `LeaseRejected` | same work tuple, `reason` | worker stops work |
| `Event` / `ReplayEvent` | `eventSeq`, `taskId`, `event` | ordered durable event |
| `EventAck` | `eventSeq` | advances contiguous cursor only |
| `ReplayRequest` | `afterEventSeq`, `limit` | asks ordered replay |
| `ResyncRequired` | `replayFloor`, `snapshot` | invalidates old cursor |
| `CancelWork` | same work tuple, `operationId`, `reason` | requests cooperative cancel |
| `Quarantine` | `machineId`, optional `agentId`, `reason`, `quarantineEpoch` | revokes route/leases |
| `Wake` | `workId`, `wakeGeneration`, `reason`, `hopsRemaining` | deduplicated notification only |
| `Ping` / `Pong` | `nonce` | no execution effect |
| `Error` | `code`, `message`, optional `frameId`, `retryAfterMs` | no implicit retry |

The task/attempt/work/agent tuple and `leaseEpoch` are mandatory for all worker
execution frames. A terminal `outcome` is one of `Succeeded`, `Failed`, or
`Canceled`. `result` and `failure` are mutually exclusive; a failure has a stable
non-secret error code and optionally an operator-safe message. A Receipt is
idempotent. WorkStarted/Progress/Terminal duplicate the same durable outcome only;
a conflicting duplicate is `StateConflict`.

The only work-state transitions are `Queued -> Leased -> Running -> {Succeeded,
Failed, Canceled}`; `Leased|Running -> WaitingInput`; `WaitingInput -> Running` on
a durable authorized resume; and `Leased|Running|WaitingInput -> Recovering` on
lease expiry, restart, or delivery uncertainty. Recovery either fences/reconciles
the work to its recorded state or creates a new WorkId in `Queued`; it never moves
a terminal work backward. `Quarantined` is an administrative terminal state for
that WorkId. An Attempt is `Active` while any work is nonterminal, `Retrying` only
while a new WorkId or Attempt is being created, and terminal only after its final
work is terminal. An invalid or unfenced transition is rejected without producing
a task event.

### Sequence, ACK, cursor, and reconnect

`seq` is per authenticated connection direction and strictly increases. `ack`
acknowledges received *transport frames* and permits bounded outbound-log cleanup;
it does not mean event processing or work delivery. `cursor` and `EventAck`
acknowledge durable Hub event sequences. `Receipt` is the only signal that changes
a work delivery state to Delivered.

On connection loss after durable outbound insertion but before a matching Receipt,
the Hub records `DeliveryUnknown`. It persists the frame and correlation, fences
before replacement, and reconciles instead of blind retry. Before insertion or
when admission rejects it, state is `NotDelivered`. A Hub reconnect sends Hello
with its last cursor, then either receives events `> cursor` in order or a
ResyncRequired response. On resync, retrieve the supplied durable task snapshot,
set cursor to its `eventSeq`, and only then request later events.

## Standard A2A projection

Public A2A stays on `POST /api/v1/a2a` and SSE. `Task.id` is TaskId. RsClaw must
not expose internal WorkIds as standard task IDs or allow a caller to choose an
AttemptId/WorkId. `GetTask`, `ListTasks`, `SubscribeToTask`, and push notification
use the durable Hub task snapshot/events.

| Internal condition | A2A state | Required projection |
|---|---|---|
| Task/Attempt pending, work queued/leased/running/recovering/retrying | `TASK_STATE_WORKING` | current safe status; never report complete |
| Wait for user input/auth | `TASK_STATE_INPUT_REQUIRED` / `TASK_STATE_AUTH_REQUIRED` | retain TaskId and owner |
| terminal success | `TASK_STATE_COMPLETED` | final artifact/result |
| terminal cancellation | `TASK_STATE_CANCELED` | cancellation message |
| permanent failure or retry exhausted | `TASK_STATE_FAILED` | stable safe failure message |

`DeliveryUnknown`, `Quarantined`, lease epochs, and route data are not standard
A2A states. Authorized RsClaw clients may receive opaque diagnostics under
`metadata.rsclaw`, but interoperability clients must work without them.

## TypeScript contract

```ts
export type CanonicalId = string;
export type DeliveryState = "NotDelivered" | "DeliveryUnknown" | "Delivered";
export type WorkState = "Queued" | "Leased" | "Running" | "WaitingInput" |
  "Recovering" | "Succeeded" | "Failed" | "Canceled" | "Quarantined";
export type RelayKind = "Hello" | "HelloAck" | "RegisterAgent" | "Reconcile" |
  "DispatchWork" | "Receipt" | "WorkStarted" | "WorkProgress" | "WorkTerminal" |
  "RenewLease" | "LeaseGranted" | "LeaseRejected" | "Event" | "EventAck" |
  "ReplayRequest" | "ReplayEvent" | "ResyncRequired" | "CancelWork" |
  "Quarantine" | "Wake" | "Ping" | "Pong" | "Error";

export interface RouteEntry { machineId: CanonicalId; routeEpoch: number }
export interface RelayFrame {
  frameId: CanonicalId; fleetTeamId: CanonicalId; machineId: CanonicalId;
  sentAt: string; kind: RelayKind; seq: number; ack: number; cursor: number;
  route: RouteEntry[]; body: Record<string, unknown>;
}
export interface RelayHelloBody {
  protocol: "rsclaw.a2a.relay.v1"; authorityEpoch: number;
  auth: unknown; devMode: boolean;
}
export interface DurableTaskView {
  taskId: CanonicalId; attemptId?: CanonicalId; workId?: CanonicalId;
  agentId?: CanonicalId; deliveryState?: DeliveryState; workState: WorkState;
  eventSeq: number; a2aState: string; resyncRequired: boolean;
}
```

## WebSocket event format

The relay WebSocket has binary/text WebSocket framing supplied by TLS/WebSocket,
but every application message is one JSON `RelayFrame` above. It is distinct from
operator WebSocket v3. Backend implementation must register these operator-facing
observation events in `src/events.rs` before emitting them:

```ts
export type DurableExecutionWsEvent =
 | { event: "a2a.execution.updated"; data: DurableTaskView }
 | { event: "a2a.execution.resync_required"; data: { fleetTeamId: string; replayFloor: number } }
 | { event: "a2a.worker.quarantined"; data: { machineId: string; agentId?: string; reason: string } };
```

These observation events contain no payload, lease token, private key, or secret.
They are emitted only after the corresponding Hub transaction commits. A UI that
receives `resync_required` must refresh the snapshot before treating later updates
as ordered.

## Errors and fail-closed limits

```rust
pub enum DurableError {
    UnsupportedRelayProtocol, SchemaMismatch, AuthenticationFailed, AuthorizationDenied,
    InvalidFrame, InvalidIdentifier, InvalidTimestamp, UnknownKind,
    IdempotencyConflict, OperationExpired, StateConflict, Fenced,
    LeaseExpired, LeaseTokenInvalid, DeliveryUnknown, ReplayGap,
    ResyncRequired, RouteLoop, HopLimitExceeded, WakeSuppressed,
    Quarantined, AuthorityFenced, StandbyReadOnly, LimitExceeded,
    QueueSaturated, PayloadTooLarge, EventTooLarge, ArtifactTooLarge,
    ReplayTooLarge, RateLimited, StorageUnavailable, RecoveryRequired,
}
```

Limits are Hub-advertised in `HelloAck.limits` and locally configured with hard
upper bounds. Initial mandatory caps: route entries <= 8; hops <= 4; agent
registrations/lease <= 256; live routes <= 4,096; writer queue <= 256 frames;
replay page <= 1,000 events; frame/payload/artifact/concurrency/lease/renewal caps
must each be explicit configuration values, never unbounded defaults. A receiver
rejects an over-limit item before state mutation. Queue saturation, storage error,
or authentication uncertainty closes/requires resync rather than dropping data or
accepting unbounded buffering.

## Authentication, payload visibility, and multi-home

Production Hello authentication is a signed Ed25519 machine-key handshake bound
to `(FleetTeamId, MachineId, authorityEpoch, connection nonce)`. The Hub maps that
key to allowed MachineIds/AgentIds. Dev mode is explicitly insecure and permitted
only on loopback; it uses ephemeral keys, cannot connect to public endpoints or a
production FleetTeamId, and is visibly reported in Hello/HelloAck.

The initial `payload.encoding` is `hub-visible-json` over TLS. Hub visibility is
intentional: it supports validation, durable replay, policy, and recovery. A
future `e2e-ciphertext` encoding requires separate key-distribution and capability
negotiation; it is opaque to Hub policy/content indexing and must not be enabled
implicitly.

A machine can hold active and standby connections only to endpoints asserting the
same FleetTeamId and one accepted AuthorityEpoch. It sends/accepts commands only
from the highest valid, Hub-approved epoch and stops execution while authority is
ambiguous. This is single-master multi-home, not active-active or multi-master.

## Required implementation verification

Tests must prove atomic operation dedupe and digest conflict; persisted delivery
classification; duplicate receipt/frame safety; stale fence rejection; ordered
replay and ACK gap behavior; compacted cursor resync; Hub and worker crash
recovery; lease expiry/reconciliation/quarantine; trace/wake limits; each resource
limit fail-closed; auth and loopback-only dev restrictions; A2A status projection;
and a manual passive-standby promotion drill that verifies prior-authority fencing.
