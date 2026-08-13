# ADR 0002: A2A authenticated WebRTC direct transport

- **Status**: Accepted
- **Date**: 2026-07-17
- **Updated**: 2026-08-13

## Context

RsClaw agents can call another gateway through public A2A HTTP/SSE or through the
private hub/spoke relay WebSocket. HTTP requires the destination gateway to be
reachable. Hub relay works for private nodes because each spoke initiates an
outbound WebSocket, but every data frame traverses the hub.

The previous proposal described gathering a UDP STUN mapping and combining its
IP address with an HTTP listener port to form a direct WebSocket URL. That is not
a valid NAT traversal mechanism: a UDP mapping says nothing about TCP mapping or
TCP/WebSocket reachability. RsClaw must not present that design as NAT traversal.

The direct transport must preserve the existing A2A behavior:

- `RelayFrame::{Request, Response, Event, Cancel, Ping, Pong}` correlation;
- task follow-up routing and streaming cancellation;
- authenticated node identity and configured peer authorization;
- hub relay and HTTP/SSE as independent fallbacks.

## Decision

### Topology

The authenticated relay WebSocket is the control plane. A WebRTC DataChannel is
the optional direct data plane.

```text
signaling/control:
  Spoke A ---- authenticated relay WebSocket ---- Hub
  Spoke B ---- authenticated relay WebSocket ---- Hub

preferred data plane after ICE succeeds:
  Spoke A ===== DTLS/SCTP ordered DataChannel ===== Spoke B

fallback data plane:
  Spoke A ---- relay WebSocket ---- Hub ---- relay WebSocket ---- Spoke B
```

The control WebSocket carries complete SDP offers and answers. ICE candidates
inside SDP are produced by the same persistent WebRTC connection that later
carries data. The implementation uses `webrtc-rs` with:

- ICE connectivity checks over UDP;
- configured STUN servers for server-reflexive candidates;
- configured TURN servers for relayed candidates;
- DTLS authentication/encryption;
- reliable, ordered SCTP DataChannels.

There is no `/a2a/peer/ws` endpoint and no direct bearer token in a URL.

### Signaling protocol

`RelayFrame` adds these control-plane frames:

```rust
PeerOffer {
    session_id: String,
    target_node: String,
    sdp: String,
    signature: Option<String>,
}
PeerOfferRelay {
    session_id: String,
    source_node: String,
    sdp: String,
    signature: Option<String>,
}
PeerAnswer {
    session_id: String,
    target_node: String,
    sdp: String,
    signature: Option<String>,
}
PeerAnswerRelay {
    session_id: String,
    source_node: String,
    sdp: String,
    signature: Option<String>,
}
PeerConnected {
    peer_node: String,
    session_id: String,
}
```

The hub derives `source_node` from the already authenticated spoke connection;
it never accepts a source identity from the frame. An offer session is bound to
`(session_id, source_node, target_node)` for 60 seconds. Answers with an unknown,
stale, reversed, or mismatched tuple are rejected. SDP is limited to 64 KiB.
Duplicate session IDs are rejected.

Configured Ed25519 peers sign this exact payload:

```text
rsclaw.a2a.webrtc.v1\n{session}\n{source}\n{target}\n{kind}\n{sdp}
```

The receiver verifies the signature against the configured public key before
applying SDP. Peers without a configured public key still rely on authenticated
hub identity and explicit `agents.a2a[].nodeId` allowlisting; that is a weaker
compatibility mode because the hub can substitute signaling. Deployments that
require end-to-end peer authentication configure Ed25519 keys on both peers.
Unknown, self, and revoked relay nodes are rejected by the existing relay
authentication boundary.

A deterministic lexical node-ID rule chooses the offerer. This prevents normal
offer glare. Signaling is currently non-trickle: gathering completes before the
full SDP is sent.

### DataChannel behavior

The direct channel carries JSON-encoded `RelayFrame` values. It is configured as
ordered and reliable. Frames over 16 KiB are split into bounded SCTP messages and
reassembled. The total reassembled frame limit is 8 MiB. Invalid chunk indices,
counts, duplicate storage growth, or oversized frames are rejected.

A direct connection is registered only after the DataChannel opens. Registration
has a monotonically increasing generation, so teardown from an older connection
cannot remove its replacement. Route leases received over the direct channel
must advertise only `peer_node/agent` references. Direct routes and pending
requests are removed when the owning generation closes. Heartbeats use
`RelayFrame::Ping/Pong`.

Direct request identity is bound to the authenticated peer node. The untrusted
`principal` string in a data frame is not used as the caller identity.

### Route ownership

Direct connectivity is local to one source gateway and one target node. It is
never global hub state. The hub route table remains `Relayed`; a direct link from
A to B must not suppress hub fallback for C to B or even for A after its direct
link fails.

The outbound order is:

1. local `AgentRegistry` in-process dispatch;
2. established local `PeerManager` DataChannel;
3. the source gateway's authenticated spoke control connection, with the hub
   forwarding `RelayFrame::Request/Response` to the destination spoke;
4. configured public A2A HTTP/SSE through `A2aClient`.

`rsclaw-agent` does not depend on `rsclaw-runtime`. Runtime installs the narrow
`rsclaw_types::OutboundA2aHost` interface. The agent derives the canonical target
from `agents.a2a[].nodeId` plus `remoteAgentId` (default `main`). `Ok(None)` from
the host means no runtime route exists and permits HTTP/SSE fallback. ACL,
protocol, or completed remote errors are surfaced rather than silently bypassed.

The runtime-owned agent-tool route currently uses unary `SendMessage`; HTTP
fallback retains its existing streaming progress behavior. Relay protocol paths
also support `SendStreamingMessage` and `SubscribeToTask`, and task follow-ups
consult direct task routes before hub task routes. A future cross-crate stream
host may preserve progress events without changing dependency direction.

Automatic direct-to-hub retry occurs only when a direct frame was not queued. A
response timeout or post-send channel loss has an unknown delivery outcome and
is surfaced without retry, preventing duplicate execution of non-idempotent A2A
methods.

### Hub authorization

Each spoke authenticates to the hub with its configured token or Ed25519
challenge-response. Signaling and spoke-originated relay requests are processed
only after authentication completes. Keypair spokes do not send route leases,
SDP, or outbound fallback requests before completing the challenge.

Direct DataChannel requests are checked against the same configured source
node `a2a:invoke:<target>` scopes; the frame's claimed principal cannot widen
that authority.

The hub enforces:

- the connection's source node identity;
- `relay:connect` and `relay:advertise` scopes for leases;
- `a2a:invoke:<target>` for spoke-originated cross-node calls;
- target route ownership and source/target separation;
- forwardable A2A JSON-RPC methods only;
- response correlation back to the authenticated source spoke.

The target spoke rewrites `node/agent` to its local agent ID only after validating
that the target node is itself.

### ICE, STUN, TURN, and reachability

STUN is part of ICE and discovers UDP server-reflexive candidates for the live
WebRTC socket. It does **not** prove direct reachability and does not create a TCP
or WebSocket mapping.

Direct ICE may work for LAN peers and many NAT combinations. It is not guaranteed.
Symmetric NAT, CGNAT, enterprise firewalls, UDP blocking, endpoint-dependent
filtering, or policy may prevent a direct pair. A configured TURN service is
required for robust WebRTC connectivity in those environments. TURN is itself a
media/data relay and is therefore not physically peer-to-peer, although it keeps
the A2A application data off the RsClaw hub.

If ICE/TURN negotiation fails, RsClaw keeps hub relay eligible. If the hub route
is unavailable, configured HTTP/SSE remains the final fallback. No success-rate
claim is made without deployment measurements.

### Configuration

```json5
{
  gateway: {
    a2a_relay: {
      mode: "spoke",
      nodeId: "node-a",
      hubUrl: "wss://hub.example.com/api/v1/a2a/relay/ws",
      privateKeyFile: "/run/secrets/node-a-ed25519",
      peer: {
        enabled: true,
        stunUrls: ["stun:stun.example.com:3478"],
        turnUrls: ["turn:turn.example.com:3478"],
        turnUsername: "node-a",
        turnCredential: { source: "env", id: "RSCLAW_TURN_CREDENTIAL" },
        // 0 or omitted: let the OS choose the local ICE UDP port.
        listenPort: 0,
      },
    },
  },
  agents: {
    a2a: [
      {
        id: "peer-b",
        url: "https://peer-b.example.com", // HTTP/SSE fallback
        nodeId: "node-b",
        remoteAgentId: "main",
        publicKey: "base64-ed25519-public-key",
        scopes: ["a2a:invoke:node-a/main"],
        authToken: "${PEER_B_HTTP_TOKEN}",
        description: "Peer B capabilities",
      },
    ],
  },
}
```

Hub node configuration must grant source spokes the desired
`a2a:invoke:<node/agent>` scopes in addition to relay connect/advertise scopes.
A local `agents.a2a` peer declaration may override that inbound direct grant
through its `scopes` list. Omitting `scopes` retains the matching configured-node
grant for compatibility; an explicit empty list denies all direct invocation.
Secrets are not logged, and SDP is not logged because it may contain network
addresses or TURN-derived information.

## Verification requirements

Required automated coverage includes:

- signed offer/answer rejection for unknown, malformed, stale, replayed, and
  mismatched sessions;
- real relay WebSocket source-to-target request and response correlation;
- two local WebRTC peers exchanging unary, streaming, terminal, and cancel
  frames;
- a failed/lost direct path leaving hub relay usable;
- unrelated source gateways retaining their hub route when another pair is
  direct;
- task-ID follow-ups preferring direct task routes, then hub routes;
- stale generation teardown preserving a replacement connection;
- frame chunk boundaries, the 8 MiB frame limit, and bounded aggregate
  reassembly memory;
- HTTP/SSE fallback when neither direct nor hub route is available.

Tests that merely connect in-process `mpsc` channels do not establish ICE,
DTLS/SCTP, authenticated signaling, or NAT traversal and must be labelled as unit
transport tests rather than end-to-end proof.

## Consequences

### Positive

- Uses a standards-based UDP NAT traversal stack instead of pretending UDP STUN
  establishes TCP/WebSocket reachability.
- Direct application data bypasses the RsClaw hub when ICE selects a direct pair.
- Existing `RelayFrame` correlation, route leases, task routing, cancellation,
  and hub fallback are reused.
- Direct reachability remains source-local, avoiding global route poisoning or
  fallback suppression.
- DTLS/SCTP supplies encryption, reliability, ordering, and congestion behavior.

### Negative

- `webrtc-rs` adds ICE, DTLS, SCTP, and supporting dependencies.
- TURN infrastructure and credentials are needed for robust hostile-NAT or
  UDP-restricted deployments.
- Non-trickle SDP increases setup latency and may need bounded renegotiation or
  ICE restart work later.
- Runtime-owned direct/hub dispatch currently returns only the final unary text;
  HTTP/SSE is still the richer progress-streaming path.

### Operational risks

- Candidate and SDP data can reveal network topology; logging is prohibited.
- TURN may have material bandwidth cost.
- The authenticated hub remains required for initial signaling and as a fallback.
- A process-global host injection follows existing RsClaw inversion patterns but
  is not suitable for multiple independent gateways in one process.

## Alternatives considered

### Direct HTTP or WebSocket

Useful for publicly reachable, LAN, or Tailscale endpoints, but not a general NAT
traversal solution. Rejected as the implementation of this ADR; retained as the
HTTP fallback and as an operational deployment option.

### Custom UDP protocol

Would require independently implementing connectivity checks, authentication,
reliability, ordering, fragmentation, retransmission, congestion control, and
NAT behavior. Rejected in favor of WebRTC DataChannels.

### Hub-only relay

Reliable and retained as fallback, but keeps all application data and bandwidth
on the hub. It does not meet the direct-data-plane goal.

### Tailscale/WireGuard

Operationally simple when both nodes can join the same overlay and remains a
recommended deployment option. It is external infrastructure rather than the
portable built-in transport selected here.
