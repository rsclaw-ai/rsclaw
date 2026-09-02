# 0003 — Durable A2A distributed execution

## Status
Accepted — 2026-09-02

## Context

The current relay is a connection-correlated transport: a socket loss can turn an
already-forwarded request into a terminal route-loss failure. That is insufficient
for restart-safe execution, unambiguous retries, or a fleet whose workers can be
replaced. Its **frame schema** is replaced in place while retaining the existing
`rsclaw.a2a.relay.v1` Hello `protocol` discriminator. There is no compatibility
parser or version negotiation: an old-schema node and a durable-schema node are
intentionally incompatible even though both identify with that discriminator.

Google A2A remains the public interoperability surface, but its `Task` is not a
sufficient internal execution record: A2A has no first-class attempt, work lease,
worker fence, delivery certainty, or durable event cursor. RsClaw therefore needs
an internal execution model and a deliberately lossy standard-A2A projection.

## Decision

### Authority and topology

One Hub is the sole authority for a fleet and owns the durable redb execution
store. The initial implementation is one Hub process with local redb. A configured
standby is passive: it receives no writes and must not accept worker leases or
client mutations. Promotion is an explicit, fenced operational procedure after
confirming the old authority cannot write. This ADR does **not** define
active-active replication, consensus, multi-master writes, or automatic failover.

All commands that change execution state are committed by the Hub before their
success is reported. Workers execute only an issued `WorkId` under a Hub lease;
they do not authoritatively complete a task. A relay connection is a transport
attachment, not an execution owner.

### Identity and truth model

The durable identity chain is:

```text
FleetTeamId / RepoId / WorkspaceId / TaskId / AttemptId / WorkId / AgentId
```

* `MachineId` is a stable, installation-scoped random identifier kept in the
  machine data directory; it is never regenerated merely because a process or
  connection restarts.
* `RepoId` is a stable repository identity, configured or persisted at repository
  initialization; a path, Git remote, or branch is only evidence, never the ID.
* `WorkspaceId` is a stable worktree/workspace identity scoped to `RepoId`; a path
  is mutable metadata.
* `FleetTeamId` identifies the Hub-governed fleet and is present on every
  machine/workspace registration and relay frame.
* `TaskId` names one user-visible logical request and is immutable after creation.
* `AttemptId` names one durable try for a Task. A retry creates a new AttemptId;
  it never reuses one.
* `WorkId` names one dispatch of an Attempt to one Agent. Re-dispatch after lease
  expiry or quarantine creates another WorkId, even if it uses the same agent.
* `AgentId` identifies a registered executable capability, scoped by MachineId
  and generation. It is not an A2A `agentId` string nor a connection ID.

The Hub record is truth. Standard A2A `Task.id == TaskId` and exposes only the
current attempt's externally meaningful status/artifacts. `contextId` is stable
for the task's conversation, but is not an attempt or work identity. Internal IDs
must not be inferred from, accepted from, or overwritten by a standard A2A Task.
A2A `metadata.rsclaw` may expose opaque IDs and delivery diagnostics only to an
authorized RsClaw client; absence of that metadata must not change A2A behavior.

The canonical mappings are: queued/leased/running/recovering ->
`TASK_STATE_WORKING`; waiting-input -> `TASK_STATE_INPUT_REQUIRED` (or
`TASK_STATE_AUTH_REQUIRED`); succeeded -> `TASK_STATE_COMPLETED`; canceled ->
`TASK_STATE_CANCELED`; permanently failed, quarantined without a replacement, or
exhausted retry budget -> `TASK_STATE_FAILED`. A retrying failure is still
`WORKING` and must include a non-secret status message. A2A has no state for
`DeliveryUnknown`, fencing, or quarantine; those remain internal metadata.

### Exactly-once commands, not exactly-once effects

Every mutating operation carries `(FleetTeamId, OperationId, actor)` where
`OperationId` is an opaque canonical UUID v4 generated once by the initiator. The Hub
keeps an idempotency record keyed by `(actor, operation kind, OperationId)` with a
canonical request digest and final/accepted result. Repeating the same key and
digest returns the recorded result; the same key with a different digest is
`IdempotencyConflict`. Records survive restart and are retained at least as long
as the advertised operation-retention period. IDs are never silently reused after
compaction.

This supplies at-most-once Hub state transitions, not exactly-once external tool,
LLM, or agent effects. A timed-out or disconnected caller must query operation or
Task state rather than submit a new operation ID. Retry policies apply only after
a durable decision and never reinterpret an uncertain execution as not run.

### Relay frame and delivery contract

The sole durable relay wire format is the UTF-8 JSON object `RelayFrame` specified
in `docs/interfaces/a2a-durable-execution.md`. Its Hello `protocol` discriminator
is exactly `rsclaw.a2a.relay.v1`; that retained discriminator does not preserve the
old schema. Every frame has required envelope fields `frameId`, `fleetTeamId`,
`machineId`, `sentAt`, `kind`, `seq`, `ack`, `cursor`, `route`, and `body`; the
Hello body additionally has the required `protocol` discriminator. Authentication
binds the authenticated machine to `machineId`; receivers reject a different
claimed ID. Unknown required
fields, missing fields, noncanonical IDs, unknown kinds, invalid state transitions,
and frames over a configured limit are rejected without execution.

For an accepted `DispatchWork`, delivery is one of:

* **NotDelivered** — the Hub did not durably place the frame in the selected
  connection's outbound log (including admission/queue rejection). Re-route may
  be attempted with the same WorkId only if no worker delivery record exists.
* **DeliveryUnknown** — the Hub durably placed it in an outbound log but lacks a
  matching worker receipt before disconnect/deadline. It must not blindly create
  or run another work item. Recovery fences the old work, queries the worker when
  reachable, then either accepts its fenced result or creates a new WorkId.
* **Delivered** — an authenticated worker durably acknowledged receipt for that
  `(WorkId, leaseEpoch)`; execution may still fail. Delivered never means
  completed.

`Receipt` changes the delivery state to Delivered. `WorkStarted`, progress, and
terminal frames must carry the same `WorkId`, `AttemptId`, `AgentId`, and
`leaseEpoch`. A duplicate frame is acknowledged but produces no second transition.

### Events, replay, and retention

The Hub assigns a monotonic `eventSeq` to every task event in the same redb
transaction as the state change. Consumers ACK the greatest contiguous sequence
processed and reconnect with that cursor. The Hub replays `eventSeq > cursor` in
order; ACKs are monotonic and cannot advance beyond emitted events. Gaps require a
`ResyncRequired` response, after which the client obtains a task snapshot and a
new cursor before continuing.

Compaction retains a per-fleet replay floor and a task terminal snapshot/artifact
manifest. It may delete event payloads only below an acknowledged/expired
retention boundary. A cursor below the floor never receives a partial replay; it
gets `ResyncRequired { replayFloor, snapshotUrlOrMethod }`. Compaction never
deletes idempotency records still inside their retention window or a nonterminal
execution record.

### Worker ownership and recovery

A worker may act only with an unexpired lease issued by the Hub. Leases contain
`WorkId`, `AgentId`, `leaseEpoch`, `expiresAt`, and an opaque lease token bound to
the authenticated MachineId. Renewals are idempotent and may extend only the
current epoch. On reassignment, recovery, or quarantine the Hub advances the
fence; every write from an older epoch is rejected as `Fenced` and cannot alter
Task state or artifacts.

A worker that misses renewal becomes suspect; its work enters recovery, not an
immediate duplicate retry. The Hub records `DeliveryUnknown` where applicable,
fences the expired lease, and uses the recovery procedure above. Repeated protocol
violations, invalid signatures, resource-limit breaches, or stale-fence writes
place the MachineId/AgentId in quarantine. Quarantine revokes leases and routes;
it is cleared only by an explicit authenticated Hub operation after a new
registration/health check. A quarantined worker receives no work.

On Hub restart, redb is replayed before accepting traffic: rebuild operation
records, event high-water marks, route registrations, leases, and nonterminal
work. Leases whose wall-clock expiry has passed are fenced. For remaining work the
Hub reconnects/reconciles by WorkId and epoch before dispatching replacements.
On worker restart, the worker re-registers, presents its durable local work ledger,
and asks the Hub to reconcile; it must not resume an old lease autonomously.

### Routing, security, and payloads

Route traces contain at most 8 entries and a hop limit of 4. Each forward appends
an authenticated `(machineId, routeEpoch)` entry, decrements hops, and rejects
cycles, exhausted hops, or a repeated `(MachineId, WorkId)` wake. Wake requests
are deduplicated by `(WorkId, wakeGeneration)` and rate-limited; they cannot
create work or extend a lease. All route/wake dedupe tables are bounded and expiry
creates an observable recovery event rather than unbounded state.

Every table, socket queue, frame, event batch, payload, trace, lease duration,
renewal rate, concurrent work count, artifact size, and replay request has a
configured hard maximum. Over-limit, malformed, unauthenticated, or impossible
state is fail-closed: no execution or state mutation occurs. If continued delivery
cannot be proven, the peer is told to resync; implementations must not silently
truncate required protocol data.

Production machine authentication uses a per-machine Ed25519 key protected by
normal OS secret-storage/file permissions. The Hub verifies a signed handshake
and authorization against FleetTeamId, MachineId, and allowed AgentIds; bearer
relay identity is not permitted in production. `--dev` may enable ephemeral
loopback-only machine keys and an explicitly marked insecure mode. It must reject
non-loopback bind/relay URLs, cannot join a non-dev FleetTeamId, and cannot be
enabled by an environment default in production configuration.

The Hub can see routing metadata, identities, task state, and ciphertext sizes.
The initial payload mode is Hub-visible TLS payloads so it can validate, persist,
replay, inspect policy, and recover tasks. End-to-end encrypted payloads are a
future explicit capability: opaque ciphertext may be relayed and stored, but
precludes Hub content validation/search and requires client-visible limitations.
No implementation may claim E2E confidentiality merely because TLS is used.

A machine may be multi-homed to multiple Hub URLs only as active connection plus
standby endpoints for the **same FleetTeamId and single current Hub authority**.
It may not accept concurrent commands from two authorities. During promotion it
stops work until it verifies the promoted Hub's signed authority epoch; stale
Hub epochs are fenced. Different fleets require distinct MachineIds/credentials
or an explicit separate registration; routes and execution logs never merge.

### Delivery milestones

**Milestone 1 — durable single-Hub dispatch (first implementable release).** Ship
one authoritative Hub with local redb; stable MachineId/RepoId/WorkspaceId/
FleetTeamId; durable TaskId -> AttemptId -> WorkId -> AgentId records;
idempotent Create/Resume/Cancel operations; persisted outbound DispatchWork and
Receipt with NotDelivered/DeliveryUnknown/Delivered classification; a basic
unexpired worker lease and stale-epoch rejection; and durable Task snapshots that
project to existing A2A GetTask/ListTasks/SSE. Replace the existing RelayFrame
schema atomically while retaining Hello `protocol: "rsclaw.a2a.relay.v1"`. The Hub
and every worker must upgrade in the same maintenance window: old-schema frames
using that discriminator are rejected before route registration or dispatch. M1
uses current authenticated transport only; it does not ship passive standby,
production machine-key replacement, event replay/compaction, quarantine, or
multi-home execution.

**Later stages, in order.**

2. Add event sequence ACK/cursor replay, replay floors, compaction, and mandatory
   resync snapshots.
3. Add lease renewal, full fencing/reconciliation, worker durable ledgers, Hub and
   worker restart recovery, quarantine, and bounded route/wake controls.
4. Replace production relay credentials with machine-key authentication and the
   separately visible restricted loopback dev mode.
5. Add passive-standby configuration and a manually fenced promotion drill.
   Do not advertise HA or multi-master until a separately accepted
   replication/fencing design exists.

### Verification requirements

Implementation cannot be declared complete without automated crash/restart tests
at every commit boundary; duplicate-operation and mismatched-digest tests;
NotDelivered/DeliveryUnknown/Delivered tests; receipt/result duplicate and stale
fence tests; event gap/ACK/replay/compaction-resync tests; lease renewal/expiry,
worker restart, Hub restart, and quarantine tests; route loop/hop/wake abuse
limits; all hard-limit fail-closed tests; production-key versus restricted-dev-mode
tests; A2A projection tests; and a manual fenced passive-standby promotion drill.

## Consequences

The first durable deployment has a single write authority and therefore a planned
availability limitation during Hub failure/promotion. In exchange it has explicit
truth ownership, replayable state, safe uncertainty handling, and a path to later
replication without pretending that a WebSocket reconnect supplies durability.
Existing old-schema relay clients must upgrade together; despite retaining the
`rsclaw.a2a.relay.v1` discriminator, they are rejected rather than silently
interpreted or downgraded.
