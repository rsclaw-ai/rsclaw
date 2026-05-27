# Memory — three-tier, decay-aware, hybrid recall

RsClaw's long-term memory is built so you never call a "save_memory" tool by hand. Every relevant turn the runtime extracts durable signal, decays old signal, and auto-injects the top hits into the LLM's context.

For the high-level pitch see the [README](../README.md#memory--three-tier-decay-aware-hybrid-recall). This doc is the architecture + ops manual.

For the original design rationale and tier transitions: [`docs/memory-extraction-redesign.md`](memory-extraction-redesign.md).

---

## Document model

Every memory entry is a `MemoryDoc` (`src/agent/memory.rs`):

```
id              UUID (deterministic per content)
scope           e.g. "agent:main", "agent:main:cron", "global"
kind            entity | preference | fact | procedure | project_state
                | relationship | lesson | failure | note | summary | session
text            the actual content, original language preserved
vector          embedding (1024 d, computed at write)
tier            Core | Working | Peripheral
importance      0.0–1.0 (kind-defaulted, can be bumped)
pinned          bool — pinned docs never decay, never demoted
access_count    bumped on every search hit
created_at, accessed_at   UNIX seconds
abstract_text   L0 — one-sentence summary (optional)
overview_text   L1 — 2-3 line key-points (optional)
tags            freeform lifecycle labels ("crystallized", "merged", …)
```

Scope is the filter for recall — sub-agents and channels live in their own scopes by default. `default_memory_scope(agent_id, channel)`:

- normal chat / a2a → `agent:<id>`
- heartbeat / cron / system → `agent:<id>:<channel>` (isolated)

You can pass an explicit `scope` to the `memory_*` tools; agents writing global facts should use `"global"`.

---

## Three tiers

| Tier | Decay β | Decay floor | Purpose |
|---|---|---|---|
| **Core** | 0.8 (sub-exponential) | 0.9 | Identity-level. Your name, contact, pinned facts. Never demoted. |
| **Working** | 1.0 (standard exponential) | 0.3 | Active context. Default tier for new writes. |
| **Peripheral** | 1.3 (super-exponential) | 0.1 | Low signal — decays out fast. |

### Transition rules (`evaluate_tier_transition`)

Run on every write and every search-hit touch.

**Promote → Core** (any one of three paths, thresholds from `evolution.promotion`):

- `access_count >= access_only` (frequency alone)
- `importance >= importance_only` (positive feedback alone)
- `access_count >= both_access AND importance >= both_importance`

**Demote → Peripheral** when:

- `relevance_score < 0.15`, OR
- `age > 60 days AND access_count < 3`

**Promote Peripheral → Working**: `access_count >= 3 AND relevance_score >= 0.4`.

Pinned docs are always Core, immune to demotion.

---

## Weibull decay

Each doc's live priority is a composite of recency, frequency, and intrinsic importance, with a stretched-exponential recency curve (`relevance_score`):

```
recency    = exp( -ln(2) / effective_half_life * age_days^β )
frequency  = 1 - exp(-access_count / 5)
intrinsic  = importance
composite  = 0.4·recency + 0.3·frequency + 0.3·intrinsic
clamped to [floor, 1.0]
```

`effective_half_life = 30 days · min(exp(1.5 · importance), 10)` — important docs decay slower (capped).

The β parameter per tier (0.8 / 1.0 / 1.3) is the only thing that materially differs across tiers in the math — Core's sub-exponential β makes it "remember harder past the recent peak", Peripheral's super-exponential β makes it forget fast once age accumulates.

---

## Extraction — write side

Every user message that passes a cheap deterministic salience gate triggers a flash-model distillation pass (`src/agent/memory_extractor.rs`).

### The pre-gate

`salience_gate(text)` matches first-person markers in either language: `我叫` / `我是` / `我的` / `我喜欢` / `记住` / `我家` / `我妻子` / `my name` / `i prefer` / `remember that` / `call me` / etc. Most chit-chat and task requests fall through with zero LLM cost.

### The distillation prompt

When the gate fires, a flash model classifies the message into structured candidates:

```
entity / preference / fact / project_state / relationship / procedure
```

Each candidate gets `text` (third-person, **same language as input** — no translation), `confidence 0..1`, and a kind-derived default importance + tier. Items below `MIN_CONFIDENCE = 0.55` are dropped; over `MAX_ITEMS_PER_TURN = 6` items are truncated.

> Translation-preservation matters: extracting `我喜欢狗` into `User's favorite animal is dog` made later Chinese queries miss the BM25 token. The current prompt anchors examples in Chinese and explicitly forbids translation — see the prompt in `src/agent/memory_extractor.rs:186`.

### Failure & lesson extraction

Two side passes besides the main one:

- **`lesson`** — when the user message *corrects* the assistant ("回答不要用表格"), capture the durable rule. Core tier, not pinned (so newer corrections can supersede).
- **`failure`** — detected by tool-loop watchdog. Working tier, decays (one stuck turn isn't proof an approach is permanently bad).

### Concurrency cap

`L1_INFLIGHT` semaphore caps concurrent extractions at 4 (`try_acquire`, never wait). A burst of "remember…" messages doesn't fork-bomb. Deterministic entity capture (phone/ID/email regex) runs regardless of the cap.

---

## Retrieval — read side

### Hybrid pipeline

`build_auto_recall_bundle(agent_id, channel, query)` (`src/agent/runtime.rs:8616`) fires on every relevant user turn:

```
1. early-out if channel ∈ {heartbeat, cron, system} or auto_recall disabled
2. scope = default_memory_scope(agent_id, channel)
3. parallel: BM25 search via tantivy (top 4·k) + vector search via hnsw_rs (top 3·k)
4. RRF fuse to final top-k (default k=5, max 12)
5. tokens-budgeted concatenation into a recall context block
6. injected into the LLM request as rsclaw_hidden.recall_context
```

The recall block lands as a system-prompt-adjacent rsclaw_hidden field on the rsclaw-protocol path, and via wire-encoding on openai/anthropic providers. Each search-hit `touch()`s the doc — access_count bumps, transition re-evaluated.

### Manual recall

The LLM can also explicitly call `memory_search(query, scope?, top_k?)` (skill-style tool). Pre-parsed `/recall <q>` works without LLM.

---

## Embedders

Two interchangeable embedders behind the `Embedder` trait (`src/embed/`):

| Embedder | Where | Latency / doc | Quality | Notes |
|---|---|---|---|---|
| **BGE-small-zh-v1.5** | Candle, local | ~5 ms | Solid for zh + en | Default. 91 MB. Desktop ships bundled; CLI auto-downloads on first start (resumable). |
| **Qwen3-Embedding-0.6B** | Remote llama.cpp endpoint | ~30 ms | Higher | 1024-dim. Configure via `agents.defaults.memory.embedder.remote_url`. |

Config sketch:

```json5
{
  agents: {
    defaults: {
      memory: {
        auto_recall: true,
        recall_final_k: 5,
        retrieval: { maxTokens: 1200 },
        embedder: {
          // pick one branch:
          local: { model_dir: "~/.rsclaw/models/bge-small-zh" },
          // OR
          remote: {
            url: "http://embedder.internal:5555/v1/embeddings",
            model: "Qwen3-Embedding-0.6B",
            query_instruction: "为这个查询生成表示用于检索相关文档:",
          },
        },
      },
    },
  },
}
```

The query/document instruction asymmetry that some embedders need is supported (set `query_instruction`); leave it `None` for symmetric embedders like BGE.

---

## Tools & APIs

### In-chat (pre-parsed, zero-token)

```
/remember <text>      Save to long-term memory (Working tier, importance 0.8)
/recall <query>       Hybrid search; print top hits inline
/compact              Compress current session, summarize, save as kind=summary
```

### Agent tools

```
memory_add(text, scope?, kind?, importance?)
memory_search(query, scope?, top_k?)
memory_get(id)
memory_delete(id)
memory_pin(id)         # immune to decay/demotion
memory_unpin(id)
```

The agent picks these automatically — `memory_add` triggers on explicit "记住" / "remember that"; `memory_search` triggers when retrieval would help (and is also covered by auto_recall).

### CLI (note: reads block while gateway runs)

```bash
rsclaw memory status                 # tier distribution, scope buckets
rsclaw memory search "<query>"       # hybrid search
rsclaw memory docs --scope agent:main
```

> redb takes an exclusive lock per database file. When the gateway is up, the CLI cannot open the DB even read-only. Use the HTTP API below in that case.

### HTTP API

Authenticated by the gateway's `auth.token` (`Authorization: Bearer …`):

```
GET /api/v1/memory/stats       → { total, by_tier, by_kind, by_scope, pinned }
GET /api/v1/memory/docs?…      → list/search docs
  optional query: q, scope, kind, limit
```

Example:

```bash
curl -s -H "Authorization: Bearer $TOKEN" \
  "http://127.0.0.1:18888/api/v1/memory/stats" | jq .

curl -s -H "Authorization: Bearer $TOKEN" \
  "http://127.0.0.1:18888/api/v1/memory/docs?q=girlfriend&limit=5" \
  | jq '.docs[] | {tier, kind, text}'
```

---

## Operating tips

**Scope hygiene**

The single biggest "memory's broken" symptom is calling from a scope that doesn't match where the doc landed. Check `by_scope` in `/api/v1/memory/stats` to see what scopes have docs, and `memory_search` with explicit scope to verify.

**Pin user identity**

Pin name / phone / IDs / company so they survive Peripheral demotion forever:

```bash
# In-chat
/remember 我叫东升,生日是 1990-01-01
# Then either ask the agent to pin, or:
curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  "http://127.0.0.1:18888/api/v1/memory/<id>/pin"
```

**Disable auto_recall on heavy-context agents**

Auto-recall costs tokens. If an agent is already context-bound (large system prompt, many tools), set `memory.auto_recall: false` on that agent and rely on `memory_search` calls only when needed.

**Per-channel scope for cron/heartbeat**

Default scope already isolates `cron` / `heartbeat` / `system` channels into `agent:<id>:<channel>`. Don't override unless you specifically want cross-channel facts shared.

**Embedder switch on production**

Switching local → remote (or vice versa) does **not** require re-embedding existing docs — both sides write 1024-dim vectors and the index is the same. New writes go through the new embedder; old vectors stay. RRF fusion buffers minor distribution shifts.

---

## What's wired vs in-progress

Shipped on `dev` (some commits unpushed at write-time):

- ✅ 3-tier with Weibull decay
- ✅ Hybrid retrieval (BM25 + vector + RRF)
- ✅ Auto-recall on user turns
- ✅ L1 extractor (preference / fact / entity / procedure / project_state / relationship / lesson / failure)
- ✅ Tier promotion/demotion on access
- ✅ Pinned docs
- ✅ HTTP read API (stats + docs)
- ✅ Local BGE-small-zh + remote Qwen3 embedders

In-progress / next:

- L0 model — flash summary back into `abstract_text` on each new doc
- Recall context wire-encoding for all providers (currently rsclaw + openai + anthropic; gemini next)
- Mutating HTTP endpoints (delete, pin, importance-bump) — currently only via in-agent tools or CLI
- Cross-scope dedup pass during crystallization
