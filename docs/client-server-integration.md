# Client ↔ Server integration guide

**Audience:** anyone building a client against rsclaw-server — the
github-rsclaw agent runtime, a brand-new SDK, an evaluation
harness, or a one-off curl wrapper. Pair with:

- [`client-integration-guide.md`](client-integration-guide.md) — recent-
  changes addendum (content-addressed prefix, segment cache, `/compact`)

This doc is the **integration map** for the client side: how to
authenticate, which endpoint to pick, what request body it wants, what
SSE shape comes back, and the pitfalls collected from incidents.

---

## 1. Topology

```
your client
   │  HTTPS — https://api.rsclaw.ai  (prod, ICP-filed CN domain,
   │                                            Caddy TLS-terminates to
   │                                            rsclaw-server on :6666)
   │
   │  HTTPS — http://<dev-host>:8442           (dev convention)
   ▼
rsclaw-server (this repo) ── routes by `model` to local GPU fleet
   │                          OR external upstreams (OpenAI, Anthropic,
   │                          DeepSeek, MiniMax via their adapters).
   │
   ▼
rsclaw-llm worker(s)        (only for `rsclaw-*` models; external
                             providers go to their own APIs)
```

rsclaw-server is **a specialized proxy**, not a general gateway. Its
job is fronting the rsclaw-llm GPU fleet with kvCacheMode=2 + offering
OAI/Anthropic/Responses-compatible shims for legacy clients. If you
need cross-provider routing or BYO-key vaulting, that's a github-rsclaw
concern; don't ask rsclaw-server to do it.

---

## 2. Authentication

### 2.1 Bearer token (client key)

Every `/v1/*` request needs `Authorization: Bearer
<key>`. Keys come in two flavors:

| Source | Where defined | Tier | Quota? |
|---|---|---|---|
| `config.toml` `[[client_keys]]` | server operator | static (operator-assigned) | NO — exempt by design (legacy path) |
| Postgres `api_keys` table | console signup or operator-minted | DB-resolved | YES (calls/day + per-request input + max_tokens) |

The auth path is identical from the client's POV; the server resolves
both internally. DB-backed keys are the long-term path; static keys
exist for back-compat and get migrated away as the legacy `[[client_keys]]`
section retires.

### 2.2 BYO upstream credentials (`x-provider-key`)

For models that route to an external provider (e.g. `model:
"gpt-4o-mini"` → OpenAI), the client may attach `x-provider-key:
<their-openai-key>` and rsclaw-server uses it for the upstream call.
This is **separate** from the `Authorization` bearer (which is the
client's identity on rsclaw-server). Never put a BYO key into
`Authorization: Bearer ...`; the server will reject it as an invalid
client key.

When auth is disabled (dev fallback only — no `[[client_keys]]` and no
DB-backed keys configured), the server falls back to reading
`Authorization: Bearer ...` as a BYO key for compatibility with
clients that don't know about `x-provider-key`. Don't rely on this in
prod.

### 2.3 Tier / quota responses

DB-backed keys get three quota dimensions enforced per request:

| Dimension | When it fires | Status | Error code |
|---|---|---|---|
| Per-request input bytes | inbound body too large for the tier | **413** | `quota_exceeded` |
| `max_tokens` / `max_output_tokens` | request asks for more than the tier ceiling | **429** | `quota_exceeded` |
| Calls/day cap | already used today's quota | **429** | `quota_exceeded` |

The 413 fires at body-read time — BEFORE the JSON is deserialized —
so an oversize chunked body terminates the stream early and the
client sees `Connection: close` after the 413. Don't expect server to
read your full body and then reject; design retries accordingly.

Day boundary: 00:00 `Asia/Shanghai` (UTC+8, no DST). The error message
includes `seconds_until_reset`-equivalent timing.

vip-3 tier has `calls_per_day: 0` which is the "unlimited" sentinel.

---

## 3. Endpoint catalog

| Path | Method | Purpose | Inbound shape | Outbound shape |
|---|---|---|---|---|
| `/health` | GET | liveness | (public, no auth) | `{status,nodes,sessions,agent_sessions,models}` |
| `/v1/models` | GET | OAI-compat model list | — | OAI models envelope |
| `/v1/chat/completions` | POST | OAI Chat Completions passthrough; routes by `model` | OAI ChatCompletions request | OAI ChatCompletions stream chunks (`choices[].delta.*`) + `data: [DONE]` |
| `/v1/messages` | POST | Anthropic Messages adapter | Anthropic Messages request | Anthropic SSE stream (`message_start`, `content_block_*`, `message_delta`, `message_stop`) |
| `/v1/responses` | POST | OpenAI Responses adapter | OpenAI Responses request | OpenAI Responses object (streaming or one-shot) |
| `/v1/agent/sessions` | POST | **native** — create kvCacheMode=2 session | `{model, system?, dynamic_prefix, ...}` | `{session_id, prefix_id, n_prefix_tokens, ...}` |
| `/v1/agent/sessions/replay` | POST | rebuild a session from supplied history | `{model, history:[...], dynamic_prefix, ...}` | same as create |
| `/v1/agent/sessions/{id}/turn` | POST | **native** — incremental turn | `{user_message?, tool_results?, stream?, options?}` | rsclaw-native SSE (§4.2) |
| `/v1/agent/sessions/{id}/state` | GET | inspect session meta + history | — | `{session_id, tenant, pinned_backend, message_count, created_at, last_active}` |
| `/v1/agent/sessions/{id}/compact` | POST | in-place KV-cache splice (drop middle, keep head+tail+summary) | `{keep_head_n, keep_tail_n, summary}` | `{n_messages_removed, n_tokens_freed}` |
| `/v1/agent/sessions/{id}` | DELETE | best-effort delete; frees worker slot | — | 204 |
| `/v1/agent/fastshot` | POST | small-model, no-session lane | `{prompt, stream?, max_tokens?, options?}` | rsclaw-native SSE (§4.2) |
| `/v1/agent/oneshot` | POST | agent-model non-cached lane | same as fastshot | same as fastshot |
| `/v1/agent/vision` | POST | image+text on the vision-capable lane | `{prompt, images:[...], ...}` | same as fastshot |

---

## 4. Native session lane — the kvCacheMode=2 path

This is the lane that actually saves GPU time across turns by re-using
prefill KV state. If you care about cache hit rate, use this lane.

### 4.1 Lifecycle

```
1. POST /v1/agent/sessions
   ├─ in:  {model, system?, dynamic_prefix:{tools?,system?,user_system?}, options?}
   └─ out: {session_id:"rs_<node>_<hex>", prefix_id, canonical_id,
            n_prefix_tokens, expires_at_ms, create_ms}

2. POST /v1/agent/sessions/{session_id}/turn         (repeat per agent step)
   ├─ in:  {user_message:"..." | tool_results:[...], stream:true, options?}
   └─ out: SSE stream (§4.2)

3. POST /v1/agent/sessions/{session_id}/compact      (optional, when context grows)
4. DELETE /v1/agent/sessions/{session_id}            (when the conversation ends)
```

Each turn appends to a server-side log keyed by `session_id`. The
server is the source of truth for conversation history — the worker
holds the KV cache in GPU RAM; if it dies or evicts the slot, the
server replays from the log transparently.

### 4.2 Native SSE wire shape

Same five event types across `turn`, `fastshot`, `oneshot`, `vision`:

```
data: {"type":"delta","content":"Hello"}\n\n
data: {"type":"delta","content":", world"}\n\n
data: {"type":"thinking","content":"reasoning fragment..."}\n\n        (only on reasoning models)
data: {"type":"tool_call","id":"call_42","name":"search","input":{...}}\n\n   (whole call per frame)
data: {"type":"done","finish_reason":"stop","usage":{"input_tokens":42,"output_tokens":17}}\n\n
data: [DONE]\n\n                                                       (server-emitted sentinel)
```

Parser rules:

- Read line-buffered. SSE framing is `data: <json>\n\n`; ignore blank
  lines and lines that don't start with `data:`.
- Dispatch by `type`:
  - `delta`     → append `content` to assistant text
  - `thinking`  → append `content` to reasoning trace
  - `tool_call` → emit one complete tool call (id/name/input arrive
                  whole, not accumulated)
  - `done`      → terminal frame; carries `finish_reason` and `usage`
  - `error`     → terminal error; `{type:"error",error:{code,message}}`
- `[DONE]` after `done` is the SSE framing convention. Treat both as
  end-of-stream; the `usage` lives on `done`.
- **Unknown `type` values: don't error**, just ignore. The server may
  add new types (e.g. `cache_hit_summary`); forward-compat keeps old
  clients working.

### 4.3 Usage field names

The server forwards worker usage objects verbatim. Different lanes
use different field names — be defensive:

| Lane | Likely field names |
|---|---|
| `/v1/agent/sessions/*/turn` | `input_tokens` / `output_tokens` (rsclaw-native convention) |
| `/v1/agent/fastshot/oneshot/vision` | `input_tokens` / `output_tokens` |
| `/v1/chat/completions` (OAI passthrough to local fleet) | `prompt_tokens` / `completion_tokens` |
| Same path, external provider upstream | varies (provider-specific) |

Robust parser: try `input_tokens || prompt_tokens || input`, then
`output_tokens || completion_tokens || output`. Default missing to 0
rather than dropping the whole usage object.

### 4.4 `session_diverged` transparent recovery

When a worker can no longer honor a session (slot evicted, version
drift, restart), the server's `handle_turn` catches the
`session_diverged:` error and transparently auto-replays the history
to a fresh upstream slot. The client sees the SSE stream pause briefly
(~500 ms) then resume on the same `session_id`. **Don't write client-
side `session_diverged` recovery — the server has already handled it
before your client's first byte arrives.**

If the auto-replay itself fails (e.g. no eligible worker), the client
gets a 503. Treat 503 on `/turn` as retryable with backoff.

### 4.5 DELETE on cleanup is important

Sessions left open consume worker slots until the worker's 1 h idle
TTL fires. Workers have small slot counts (`max_slots: 8` typical) —
~5 orphaned sessions per worker can exhaust capacity. A periodic
server-side sweeper (every 5 min, evicts sessions idle ≥ 45 min)
catches the worst, but explicit DELETE is much cheaper. Always DELETE
when your conversation ends.

If you're an agent runtime that keeps sessions alive across multiple
user turns, DELETE only on actual conversation end (user goodbye, app
shutdown, idle timeout > 1 h). Don't DELETE between agent steps in a
single conversation — that defeats the cache.

---

## 5. OAI / Anthropic / Responses adapter lanes

These exist for clients that already speak OAI / Anthropic / OpenAI-
Responses and can't migrate to the native session protocol yet. The
server translates the adapter shape onto the native session lane
internally:

- `/v1/messages` (Anthropic) → server mints / resumes a hidden session
  via `previous_response_id`-equivalent fingerprinting, dispatches as
  `op:turn`, emits Anthropic-shape SSE back to the client.
- `/v1/chat/completions` (OAI Chat) → same idea, OAI-shape SSE out.
- `/v1/responses` (OpenAI Responses) → uses `previous_response_id` to
  resolve to a session; outputs the Responses object envelope.

These adapters carry the SAME tier/quota gating as the native lane.
They DON'T expose `session_id` directly — the adapter manages
identity for you (Anthropic message-array fingerprint, Responses
`previous_response_id`). If you want explicit session control + the
best cache hit rate, prefer the native lane.

**Don't add new adapter lanes here.** Inbound OAI / Anthropic / etc.
compatibility for the agent runtime belongs in github-rsclaw, not in
this server.

---

## 6. Worked examples

### 6.1 Native session, one turn, with curl

```bash
GW="https://api.rsclaw.ai"
CK="sk-<your-client-key>"

# 1. Create session
SID=$(curl -s -X POST -H "Authorization: Bearer $CK" \
        -H "Content-Type: application/json" \
        "$GW/v1/agent/sessions" \
        -d '{"model":"rsclaw-agent-v1",
             "system":"You are concise.",
             "dynamic_prefix":{"system":""}}' \
      | jq -r .session_id)

# 2. Turn (SSE streaming)
curl -s -N -X POST -H "Authorization: Bearer $CK" \
       -H "Content-Type: application/json" \
       "$GW/v1/agent/sessions/$SID/turn" \
       -d '{"user_message":"hello","stream":true}'
# data: {"type":"delta","content":"Hi"}
# data: {"type":"delta","content":"!"}
# ...
# data: {"type":"done","finish_reason":"stop","usage":{...}}
# data: [DONE]

# 3. Clean up
curl -s -X DELETE -H "Authorization: Bearer $CK" \
       "$GW/v1/agent/sessions/$SID"
```

### 6.2 Multi-turn agent loop (pseudocode)

```text
session = POST /v1/agent/sessions {model, system, dynamic_prefix}
loop:
    user_input = receive_from_user()
    on user_input:
        stream = POST /turn {user_message: user_input, stream: true}
        for event in parse_sse(stream):
            match event.type:
                "delta":     accumulate_text(event.content)
                "thinking":  accumulate_reasoning(event.content)
                "tool_call": dispatch_tool(event); collect_result
                "done":      record_usage(event.usage); break
                "error":     handle(event.error); break
        if tool_results_collected:
            stream = POST /turn {tool_results: [...], stream: true}
            # same loop
    on conversation_end:
        DELETE /v1/agent/sessions/{session.id}
```

`session_diverged` retry happens inside the server; your `POST /turn`
either succeeds normally or returns a non-`session_diverged` error.

### 6.3 Compact when the conversation grows

```bash
# Drop middle messages, keep first N + last M + a model-generated summary.
curl -s -X POST -H "Authorization: Bearer $CK" \
       -H "Content-Type: application/json" \
       "$GW/v1/agent/sessions/$SID/compact" \
       -d '{"keep_head_n": 2,
            "keep_tail_n": 6,
            "summary": "User asked about X; we tried A,B; settled on C."}'
# {"n_messages_removed": 12, "n_tokens_freed": 4321}
```

Compact is server + worker round-trip; it briefly pauses turn
dispatch on that session (per-session write lock).

---

## 7. Common pitfalls

1. **Parsing unknown `type` values as errors.** Server is allowed to
   add event types over time. Treat unknown `type` as "skip, log
   debug, keep streaming." Don't fail the turn.

2. **Confusing `[DONE]` sentinel with terminal usage.** The `done`
   event carries usage; `[DONE]` is the framing convention afterward.
   Some clients treat them as competing signals and emit double-Done
   downstream. Treat them as one terminal pair.

3. **Skipping DELETE on session cleanup.** Worker slots leak until
   1 h TTL. The server's 5-min sweeper helps but isn't a substitute.
   See §4.5.

4. **Building a custom `session_diverged` retry path.** Don't. Server
   handles it. Your retry loop won't be reentrant-safe with the
   server's auto-replay and will at best double-charge tokens.

5. **Sending `Authorization: Bearer <byo-openai-key>` instead of
   `x-provider-key`.** When auth IS enabled, `Authorization` is your
   identity on rsclaw-server. The server will reject the BYO key as
   an unknown client. Use `x-provider-key` for the upstream credential.

6. **Assuming 413 means "retry with smaller body".** It IS a retry
   signal, but only after the client splits / summarizes the prompt.
   Auto-retry with the same body will hit the same cap. Tier
   ceilings are absolute per-request bounds, not rate windows.

7. **OAI-shape parser on the native lane.** The native lane emits
   rsclaw-native SSE (`{type:"delta","content":...}`), NOT OAI shape
   (`{choices:[{delta:{content:...}}]}`). If you point an OAI-shape
   parser at `/v1/agent/sessions/*/turn`, every event drops silently
   and your agent reports "empty response" while the worker emitted
   dozens of tokens. (This is exactly the 2026-05-18 → 19 outage; doc
   misdescription on §6.2 of `ws-protocol.md` led to a client building
   the wrong parser. Now fixed.)

8. **Streaming + JSON parsing race.** `stream: true` returns SSE
   chunks; the body is NOT a single JSON object. Pass through to your
   SSE parser before trying `json.loads(response.body)`. The first
   chunk arrives before the model finishes generating; treating the
   response as a single JSON parse will block on a body that never
   closes.

9. **Reusing session_id across worker restarts via direct
   recreation.** If a worker restarts cleanly, the slot is gone but
   the session_id in the server's session_store is unchanged. Your
   next `/turn` triggers `session_diverged`-driven auto-replay
   automatically; you don't need to mint a new session_id yourself.

10. **Sending images on `/v1/agent/fastshot` instead of
    `/v1/agent/vision`.** Fastshot is text-only — it doesn't have the
    mmproj weights loaded. Send any multi-modal content to
    `/v1/agent/vision`. (Fallback: legacy clients can stuff base64
    images into chat/completions; the vision processor on the server
    translates if `vision.enabled = true`. But that's a translation
    overhead — vision lane is the direct path.)
