# A2A — agent-to-agent protocol & hub-spoke relay

RsClaw is a full [Google A2A v1.0](https://a2a-protocol.org/latest/specification/) implementation **plus** a first-class hub-spoke relay built on top. This doc covers both: the wire protocol (peers talking direct over HTTP) and the relay topology (one hub fanning out to many spokes over WebSocket).

For the user-facing pitch and a quick install demo, see the [README](../README.md#a2a--fleet-grade-agent-to-agent-routing). This doc is the operations manual.

---

## Topology choices

| Mode | Network shape | When to use |
|---|---|---|
| **Direct peer** | Caller's gateway → callee's gateway via HTTP `/api/v1/a2a`. | Two reachable boxes, simple federation. |
| **Hub-spoke relay** | Each spoke holds **one outbound WebSocket** to the hub. Hub HTTP A2A endpoint relays requests inward over the spoke's WS. | Spokes behind NAT / firewall / GFW. Heterogeneous fleet (GPU box, big-RAM box, partner box). One A2A address (the hub) for the entire fleet. |

Both modes use the same `agents.a2a[]` peer config — only the `url` and routing semantics differ.

---

## Wire protocol — endpoints

```
GET  /.well-known/agent.json               Agent Card (discovery, securitySchemes, capabilities)
GET  /api/v1/health                        Liveness probe
POST /api/v1/a2a                           JSON-RPC dispatch
                                             — sync responses for non-streaming methods
                                             — Accept: text/event-stream for SendStreamingMessage / SubscribeToTask
WS   /api/v1/a2a/relay/ws                  Spoke ↔ hub relay handshake (hub only)
```

Auth on `/api/v1/a2a` bypasses gateway-level token and goes through the dedicated `a2a_auth_layer` — see [Authentication](#authentication) below.

## Wire protocol — methods

All 11 spec methods are implemented. Most users only need the first three.

| Method | Purpose |
|---|---|
| `SendMessage` | Submit a task, block until terminal state, return the final `Task`. |
| `SendStreamingMessage` | Submit a task, stream `status-update` + `artifact-update` SSE events. |
| `SubscribeToTask` | Tap into an in-flight task's SSE stream by `taskId`. |
| `GetTask` / `ListTasks` | Read task snapshots (`ListTasks` paginated). |
| `CancelTask` | Fire the cancel token; runtime exits at the next agent-loop boundary. |
| `CreateTaskPushNotificationConfig` | Register a webhook for a task's lifecycle events. |
| `GetTaskPushNotificationConfig` / `ListTaskPushNotificationConfigs` / `DeleteTaskPushNotificationConfig` | Webhook CRUD. |
| `GetExtendedAgentCard` | Full Agent Card (extended metadata beyond `/.well-known/`). |

Quick example:

```bash
curl -sS -X POST http://127.0.0.1:18888/api/v1/a2a \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":"1","method":"SendMessage",
       "params":{"message":{"messageId":"m1","role":"user",
         "parts":[{"type":"text","text":"reply with ack"}]}}}'
# → { "result": { "status": { "state": "TASK_STATE_COMPLETED" },
#                 "artifacts": [{ "parts": [{"type":"text","text":"ack"}] }], ... } }
```

For streaming, add `Accept: text/event-stream` and switch to `SendStreamingMessage` — events arrive as SSE frames.

## Authentication

`/api/v1/a2a` is on the gateway's bypass list — gateway-level `auth.token` does NOT apply here. The route runs through `a2a_auth_layer` (`src/a2a/auth.rs`) which accepts:

1. The gateway's operator token (`gateway.auth.token`) as a bearer — admin-level identity.
2. Any configured A2A principal secret (`gateway.a2a.clients[].secret`) — scoped identity for partners.

Header must be `Authorization: Bearer <secret>` or `X-API-Key: <secret>`. Empty `Accepted` set (no operator token AND no principals) is dev pass-through — open mode. Explicitly configured credentials must resolve to non-empty values or config loading fails.

Named clients are fail-closed: an empty `scopes` list grants no operations. Supported grants are:

- `*` — all standard A2A operations;
- `a2a:method:<Method>` or `a2a:method:*` — one/all JSON-RPC methods;
- `a2a:invoke:<agent>` — `SendMessage` / `SendStreamingMessage` for an agent;
- `a2a:read:<taskId>`, `a2a:read:tasks`, `a2a:read:agent-card` — task/card reads;
- `a2a:cancel:<taskId>` — task cancellation;
- `a2a:push:<taskId>` — push-notification config operations.

Each action scope accepts `*` as its target. When `SendMessage` or `SendStreamingMessage` omits `metadata.agentId`, authorization resolves the gateway's actual default agent before matching `a2a:invoke:<agent>`; a grant for that default agent therefore works without requiring `a2a:invoke:*`. Legacy `authTokens` / `apiKeys` and their environment-list equivalents retain historical full access through an explicit internal `*` grant. Transport-authenticated direct and relay requests receive an internal `*` only after their node/target ACL succeeds, preventing the standard dispatcher from denying the same request a second time. Task ownership checks still apply after scope authorization.

Set these env vars to enforce auth without editing config:

```bash
export RSCLAW_A2A_BEARER_TOKENS="token-1,token-2"
export RSCLAW_A2A_API_KEYS="key-1,key-2"
```

Always set one before exposing to the internet. Empty + public = open.

## INPUT_REQUIRED suspend / resume

Every agent has a built-in `wait_input(prompt, auth?)` tool. When the LLM calls it mid-turn the runtime:

1. Publishes `TASK_STATE_INPUT_REQUIRED` (or `TASK_STATE_AUTH_REQUIRED` when `auth: true`) with the prompt as a `role: agent` message.
2. Suspends the turn awaiting a `SendMessage` with the **same `taskId`**.
3. Resumes by feeding the client's text back as the tool's result.

Resume protocol — client receives `TASK_STATE_INPUT_REQUIRED`, then:

```bash
curl -X POST http://127.0.0.1:18888/api/v1/a2a -H "Authorization: Bearer $TOKEN" -d '{
  "jsonrpc":"2.0","id":"r1","method":"SendMessage",
  "params":{"message":{
    "messageId":"m-resume-1",
    "taskId":"<the same taskId>",       ← critical: same id routes to resume short-path
    "role":"user",
    "parts":[{"type":"text","text":"<your answer>"}]
  }}}'
```

Streaming subscribers see the loop continue and produce the final artifact + `TASK_STATE_COMPLETED`.

## Push notifications

Register a webhook per task. The gateway POSTs every lifecycle event with HMAC-SHA256 signed payloads:

```
POST <your-url>
Content-Type: application/json
X-A2A-Signature: <base64(hmac_sha256(token, body))>
X-A2A-Task-Id:   <taskId>

{"kind":"status-update","taskId":"...","contextId":"...",
 "status":{"state":"TASK_STATE_WORKING"},"final":false}
```

Verify (Python):

```python
import hmac, hashlib, base64
expected = base64.b64encode(hmac.new(token.encode(), body, hashlib.sha256).digest()).decode()
assert expected == request.headers["X-A2A-Signature"]
```

Retry: 3 attempts with exponential backoff (2 s / 4 s / 8 s). Webhook configs persist across restarts.

## Cancellation semantics

`CancelTask` fires the task's cancel token. The runtime checks it:

- At the top of every agent-loop iteration
- Before every tool dispatch

So cancel is honored *between* tool calls, not *inside* a running LLM stream or tool. A 30-second blocking tool runs to completion; the cancel kicks in afterward. The dispatcher additionally publishes a terminal `TASK_STATE_CANCELED` event and closes the SSE stream immediately so clients aren't blocked on the long-running call.

## Persistence

Tasks (history + artifacts + push configs + status) persist to `var/data/a2a/tasks.redb` so `GetTask` / `ListTasks` and webhook registrations survive restarts.

---

## Hub-spoke relay

### Why the relay exists

A2A spec assumes peers are mutually reachable over HTTP. In practice spokes are often:

- Behind NAT — laptops, home labs, devices on a corporate LAN
- Behind a firewall — GPU boxes with no public ingress
- In mainland China — WebSocket / SSE long connections to international relays are flaky

The relay flips that: each spoke holds **one persistent outbound WebSocket** to the hub. The hub's public HTTP A2A endpoint accepts incoming requests, looks up `metadata.agentId` against connected spoke routes, and forwards through the WS. No spoke needs an inbound port.

```
         User / channel / curl
                  │
                  ▼ HTTP /api/v1/a2a
            ┌──────────────┐
            │  Hub gateway │  ← public Internet
            │  (rsclaw)    │
            └─────┬────────┘
              WS  │  relay (one persistent
                  │  conn per spoke)
   ┌──────────┬──┴──────┬──────────┐
   ▼          ▼          ▼          ▼
spoke-mac  spoke-aihub  spoke-…   spoke-partner
(laptop)   (2×4090)               (3rd-party)
```

### Spoke config

Spoke just needs the relay block — it dials out, no inbound port needed:

```json5
{
  gateway: {
    a2a: {
      relay: {
        mode: "spoke",
        nodeId: "spoke-aihub",                  // unique id within the hub
        relays: [
          "wss://hub.example.com/api/v1/a2a/relay/ws",        // primary
          "wss://backup.example.com/api/v1/a2a/relay/ws",     // standby
        ],
        strategy: "primary_standby",            // or "round_robin"
        privateKey: "<base64 ed25519 key>",     // keypair-mode auth
        // OR: token: "<bearer>"                ← legacy token-mode auth
      },
    },
  },
}
```

The spoke runs `rsclaw gateway run` like any other node — its **own** `agents.list[]` and capabilities are exactly what the hub will route to via `agent_<spoke_id>`.

### Hub config

The hub is just a gateway with declared A2A peers, each pointing back at the hub's own URL with a `remoteAgentId` of `<spoke-id>/<agent-id>`:

```json5
{
  gateway: {
    port: 18889,
    auth: { token: "<operator-token>" },        // also doubles as A2A bearer
    a2a: {
      relay: {
        // Hub mode is implicit when 'relay' is omitted on a server that
        // accepts /api/v1/a2a/relay/ws connections. Or be explicit:
        mode: "hub",
        principals: [{ id: "spoke-aihub", publicKey: "<spoke ed25519 pub>" }],
      },
    },
  },
  agents: {
    list: [{
      id: "main",
      default: true,
      system: "你是路由 agent。按用户原话命中工具描述,转发到对应 spoke。",
      model: { toolset: "minimal",
               tools: ["agent_spoke_mac", "agent_spoke_aihub", "memory", "clarify"] },
    }],
    a2a: [
      { id: "spoke_aihub",
        url: "http://localhost:18889",            // hub talks to itself
        remoteAgentId: "spoke-aihub/main",        // <node_id>/<agent_id> on the spoke side
        description: "GPU 多媒体生成: 文生图/图生视频/数字人/TTS。\
                      触发: 画/生图/视频/配音/数字人。" },
      { id: "spoke_mac",
        url: "http://localhost:18889",
        remoteAgentId: "spoke-mac/main",
        description: "通用对话 + 浏览器自动化 + 抖音/微信/飞书 channel。" },
    ],
  },
}
```

User types **"用 aihub 画一只猫"** → hub LLM picks `agent_spoke_aihub` → A2A client sends to `http://localhost:18889/api/v1/a2a` with `metadata.agentId: "spoke-aihub/main"` → hub's `streaming.rs` sees the agentId matches a connected spoke route → forwards via relay WS → spoke runs `aihub-t2i` → artifact streams back up.

### Peer description matters

For small models (Qwen-9B-class), the auto-generated tool description ("Send a task to remote agent 'X'") doesn't carry enough signal — the model won't route to the right spoke. **Put trigger keywords in `description`**: capability verbs, common phrasing, examples. The hub system prompt should also explicitly list the spokes as the only legal tools (toolset whitelist).

### Identity & ACL

Spokes authenticate to the hub via **keypair mode** (preferred, ed25519) or legacy token mode. Hub-side principal config declares each spoke's public key plus per-spoke scopes:

```json5
gateway.a2a.relay.principals = [
  { id: "spoke-aihub", publicKey: "<base64>",
    scopes: ["relay:advertise:spoke-aihub/*", "relay:receive:spoke-aihub/*"] },
]
```

The hub-side `can_invoke()` ACL gates every routed call against the caller's identity and the target node — denials are audit-logged. Operators bypassing via the gateway's own auth_token always pass. Configured relay and per-node tokens, inline private keys, and private-key files are fail-closed: unresolved or empty values stop configuration loading. Relay node IDs must be unique, and Ed25519 public/private key material must decode to raw 32-byte values before startup.

### Spoke ↔ hub frames

Once the WS is open the protocol uses framed JSON over a single connection: requests fan out (hub → spoke), responses + streaming events fan in (spoke → hub). Stream entries are reaped on terminal `Response` or on disconnect (the relay publishes `TASK_STATE_FAILED` with reason `relay route lost`). Route leases are validated atomically, limited to 256 agents per lease and 4,096 live routes per direct or hub table; hub TTLs are clamped to 30 seconds. A direct lease replaces that authenticated peer's previous route snapshot. Health monitoring + auto-reconnect with primary-standby failover are built in (`strategy: "primary_standby"` walks the `relays:` list in order).

### Multi-hop

A spoke can itself host another `agents.a2a[]` pointing at a third hub — chains work. There's no relay-aware client today; calls are always HTTP-then-relay-forward, so each hop is independent.

---

## Exposing the hub to the internet

The hub's HTTP A2A endpoint must be reachable from the public network. The relay WS endpoint must be reachable from the spokes. Most operators use the same address for both.

### International — Cloudflare Tunnel

Free, no VPS, HTTPS by default. WebSocket-friendly.

```bash
brew install cloudflared
cloudflared tunnel --url http://127.0.0.1:18889   # gives a *.trycloudflare.com URL
```

For stable URLs use a [named tunnel](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/get-started/create-remote-tunnel/) with `cloudflared tunnel route dns`.

### Mainland China — `frp` + domestic VPS

Cloudflare's edge nodes are unreliable from inside GFW for long-lived WS/SSE. Deploy [frp](https://github.com/fatedier/frp) on a domestic VPS (aliyun / tencent / huawei cloud, ~5-10 RMB/month).

**VPS (`frps.toml`):**

```toml
bindPort = 7000
auth.token = "<frp-secret>"

[[httpsVhost]]
type = "https"
listenPort = 443
customDomains = ["a2a.example.cn"]
```

**Local next to rsclaw (`frpc.toml`):**

```toml
serverAddr = "<vps-ip>"
serverPort = 7000
auth.token = "<frp-secret>"

[[proxies]]
name = "rsclaw-a2a"
type = "https"
localIP = "127.0.0.1"
localPort = 18889
customDomains = ["a2a.example.cn"]
```

`nps` and Sakura Frp 樱花穿透 are alternatives — same shape.

### Multi-tenant deployments — `rsclaw-tunnel`

For platforms hosting many rsclaw clients with shared edge auth, JSON-RPC method-aware rate limits, and full data control, see [`rsclaw-tunnel`](https://github.com/rsclaw-ai/rsclaw-tunnel). Not a Cloudflare replacement for individuals — only worth standing up when off-the-shelf tunnels don't give you protocol-aware multi-tenancy.

### Operations sanity-check

Before going public:

- [ ] `gateway.auth.token` set (gates the gateway-level admin paths)
- [ ] `RSCLAW_A2A_BEARER_TOKENS` or `gateway.a2a.clients[]` set (gates `/api/v1/a2a`)
- [ ] Spoke principals declared on the hub with scoped permissions
- [ ] All A2A peer `description` fields written with trigger keywords (not auto-generated)
- [ ] `gateway.bind` reviewed — `loopback` for local-only, `*` for tunneled

---

## Interop testing

End-to-end harness against the Google Python SDK (covers all 11 methods + the `wait_input` resume flow): [`tests/a2a_interop_python.md`](../tests/a2a_interop_python.md).
