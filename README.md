# RsClaw

> **An AI agent engine that remembers, learns, and routes across machines.**
> One 15MB Rust binary · A2A hub-spoke fleet · Three-tier memory · Vector + BM25 knowledge base · 13 channels · 15 LLM providers · OpenClaw drop-in.

[![GitHub Stars](https://img.shields.io/github/stars/rsclaw-ai/rsclaw?style=flat&logo=github)](https://github.com/rsclaw-ai/rsclaw/stargazers)
[![Crates.io](https://img.shields.io/crates/v/rsclaw?style=flat&logo=rust)](https://crates.io/crates/rsclaw)
[![Release](https://img.shields.io/github/v/release/rsclaw-ai/rsclaw)](https://github.com/rsclaw-ai/rsclaw/releases)
[![Downloads](https://img.shields.io/crates/d/rsclaw?style=flat)](https://crates.io/crates/rsclaw)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](#license)
[![Rust](https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust)](https://www.rust-lang.org/)

**🇺🇸 English** · [🇨🇳 中文](docs/lang/README_cn.md) · [🇯🇵 日本語](docs/lang/README_ja.md) · [🇰🇷 한국어](docs/lang/README_ko.md) · [More languages ▾](docs/lang/)

<p align="center">
  <img src="docs/images/en.gif" alt="RsClaw Preview" width="800" />
</p>

Most AI agents are stateless processes glued to a chat box. **RsClaw is a fleet**: every node persists structured memory, indexes a private knowledge base, and speaks [Google A2A v1.0](https://a2a-protocol.org/) — so a request typed on your laptop can fan out to a GPU spoke for image generation, a fleet node for RAG, and a remote partner agent for a specialist task, then come back with one streamed answer.

15 MB, ~20 MB RAM, single static binary. Pure Rust. No Node, no Python.

💬 [Join Community](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## Install

### Homebrew (macOS / Linux) — recommended

```bash
brew tap rsclaw-ai/tap
brew install rsclaw            # CLI
brew install --cask rsclaw     # Desktop app (macOS DMG)
```

### Other channels

```bash
# Cargo
cargo install rsclaw

# One-line installer (macOS / Linux)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash

# Windows
irm https://app.rsclaw.ai/scripts/install.ps1 | iex

# Or grab the binary from https://github.com/rsclaw-ai/rsclaw/releases
```

### First start

```bash
rsclaw setup          # initialize ~/.rsclaw/
rsclaw onboard        # interactive wizard: provider, channel, memory embedder
rsclaw start
```

First start downloads the local embedding model (BGE-small-zh, ~91 MB) into `~/.rsclaw/models/`. Resumable; pre-place the safetensors to skip. Desktop app ships it bundled.

---

## A2A — fleet-grade agent-to-agent routing

RsClaw implements the full [Google A2A v1.0 spec](https://a2a-protocol.org/latest/specification/) — streaming, push notifications, task persistence, cancellation, INPUT_REQUIRED interrupts, all 11 JSON-RPC methods — plus a **first-class hub-spoke relay** that turns a heterogeneous machine fleet into one logical agent.

### Why A2A is the headline feature

- One gateway, many backends behind it. The hub routes user requests to whichever spoke owns the capability — GPU box for image/video, big-memory box for RAG, partner box for proprietary tools.
- All spokes look like local tools to the LLM (`agent_<peer-id>`), so the model picks them naturally without orchestration code.
- Survives NAT, firewalls, and China-mainland network conditions: relay rides one persistent WebSocket from each spoke; nothing needs an inbound port.

### Topology

```
         User (chat / channel / curl)
              │
              ▼
       ┌──────────────┐
       │  Hub agent   │  ← public Internet, A2A v1.0 endpoint
       │   (router)   │
       └──────┬───────┘
        WS relay (one
        persistent conn
        per spoke)
              │
   ┌──────────┼──────────┐
   ▼          ▼          ▼
spoke-mac  spoke-aihub  spoke-partner
(your      (2×4090     (3rd-party
 laptop)    GPUs)       gateway)
```

Each spoke is just another `rsclaw gateway run` in **relay spoke mode**. The hub config declares spokes as A2A peers; the LLM running on the hub sees them as `agent_spoke_mac`, `agent_spoke_aihub`, etc., and routes by capability description.

Spoke config (one block — that's the whole setup):

```json5
{
  gateway: {
    a2a: {
      relay: {
        mode: "spoke",
        nodeId: "spoke-aihub",
        relays: [
          "wss://hub.example.com/api/v1/a2a/relay/ws",
          "wss://backup.example.com/api/v1/a2a/relay/ws",   // primary-standby
        ],
        privateKey: "<keypair>",
      },
    },
  },
}
```

Hub config — declare peers, LLM routes by description:

```json5
{
  agents: {
    a2a: [
      { id: "spoke_aihub",
        url: "http://localhost:18889",         // hub talks to itself
        remoteAgentId: "spoke-aihub/main",
        description: "GPU 多媒体生成: 文生图 / 图生视频 / 数字人 / TTS。\
                      触发: 画 / 生图 / 视频 / 配音 / 数字人。" },
      { id: "spoke_mac",
        url: "http://localhost:18889",
        remoteAgentId: "spoke-mac/main",
        description: "通用对话 + 浏览器自动化 + 抖音 / 微信 / 飞书。" },
    ],
  },
}
```

User types **"用 aihub 画一只猫"** → hub LLM picks `agent_spoke_aihub` → relay forwards over WS → aihub spoke runs `aihub-t2i` on its 4090 → image path streams back through hub to the user.

### Beyond hub-spoke

Direct peer mode also works — point `agents.a2a[].url` at any A2A v1.0 endpoint (rsclaw or otherwise):

```bash
curl -X POST http://127.0.0.1:18888/api/v1/a2a \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"jsonrpc":"2.0","id":"1","method":"SendStreamingMessage",
       "params":{"message":{"messageId":"m1","role":"user",
         "parts":[{"type":"text","text":"hi"}]}}}'
```

Spec coverage: `SendMessage` / `SendStreamingMessage` / `SubscribeToTask` / `GetTask` / `ListTasks` / `CancelTask` / push-config CRUD / `GetExtendedAgentCard`. INPUT_REQUIRED suspend-resume via the built-in `wait_input` tool. Tasks persisted to `var/data/a2a/tasks.redb` so `GetTask` and webhooks survive restart.

Exposing to the internet: Cloudflare Tunnel (international), `frp` + domestic VPS (mainland China), or [`rsclaw-tunnel`](https://github.com/rsclaw-ai/rsclaw-tunnel) for multi-tenant deployments. Always set `RSCLAW_A2A_BEARER_TOKENS` before going public — empty = dev mode = open.

→ Full protocol surface, hub-spoke ops, identity & ACL, tunnel recipes: [docs/a2a.md](docs/a2a.md).

---

## Memory — three-tier, decay-aware, hybrid recall

Long-term memory you never have to manage. Every turn, the runtime:

1. **Extracts** durable signal from your message into structured docs (entity / preference / fact / procedure / relationship / lesson / failure) via a flash-model distillation pass — original language preserved (Chinese in → Chinese out).
2. **Tiers** each doc:
   - **Core** — identity-level (your name, contact, pinned facts). Half-life decay floor 0.9; never demoted.
   - **Working** — active context. Standard exponential decay; promoted from Peripheral when frequently recalled.
   - **Peripheral** — low signal. Fast decay; demoted automatically and pruned over time.
3. **Decays** each doc by **Weibull stretched-exponential** with tier-specific β — recent + frequent + important docs score higher; old + ignored docs drop out.
4. **Recalls** via **hybrid retrieval**: BM25 keyword (tantivy) + vector cosine (hnsw_rs), fused by reciprocal-rank-fusion. Auto-injected into the LLM's context on every relevant turn — no manual recall calls needed.

### Embedder tiers

| Tier | Embedder | Latency | When |
|---|---|---|---|
| **Local** | BGE-small-zh-v1.5 (Candle, 91 MB) | ~5 ms / doc | Default. Ships with desktop, auto-downloaded on first start of CLI. |
| **Remote** | Qwen3-Embedding-0.6B (1024 d) on llama.cpp endpoint | ~30 ms / doc | Higher quality. Configure `memory.embedder.remote_url`. |

### Tools & CLI

```bash
# Inside chat — pre-parsed, zero-token
/remember <text>            # save to long-term memory
/recall <query>             # search (BM25 + vector RRF)
/compact                    # compress current session + save summary

# CLI / HTTP
rsclaw memory status        # tier distribution, scope buckets, pinned count
rsclaw memory search <q>    # BM25 + vector hybrid search
curl http://127.0.0.1:18888/api/v1/memory/stats     # JSON stats
curl http://127.0.0.1:18888/api/v1/memory/docs?q=…  # list + search
```

Scope is per-agent by default (`agent:main`) — sub-agents and channels can carve their own scopes to keep contexts isolated.

→ Tier math, extractor prompt design, embedder swap, HTTP API: [docs/memory.md](docs/memory.md). Original redesign rationale: [docs/memory-extraction-redesign.md](docs/memory-extraction-redesign.md).

---

## Knowledge Base — managed RAG, OOXML in, snippets out

A first-class persistent knowledge store separate from session memory. Use for: project docs, reference material, codebases, meeting notes, legal contracts — anything you want the agent to cite rather than summarize from training.

- **Collections** — tag-based veneer over a single embedding index. Create / list / delete from desktop UI or HTTP API; no per-collection store overhead.
- **Ingest** — drag-drop files in the desktop app or `POST /api/v1/knowledge/collections/<id>/docs`. Supports plain text, Markdown, PDF, **OOXML** (.docx / .xlsx / .pptx), HTML.
- **Search** — same hybrid BM25 + vector pipeline as memory, scoped to a collection.
- **Cite-by-default** — the agent's `knowledge_base` tool returns snippets with doc-id + offset, so replies can quote the source.

```bash
rsclaw knowledge ingest <path> --collection 会议记录
rsclaw knowledge search "Q3 revenue projections" --collection 财报

# In chat — agent auto-uses knowledge_base tool when query matches a collection
"根据 Q3 财报，毛利率怎么样?"
```

→ Collections model, ingest pipeline, retrieval (BM25 + vector + RRF + MMR), CLI/HTTP API: [docs/kb.md](docs/kb.md). Engineering deep-dive: [src/kb/README.md](src/kb/README.md).

---

## Agents — four lifetimes, four backends

| Type | Created by | Persists | Killed by |
|------|-----------|----------|-----------|
| **Main** | system | forever | nothing — main is immortal |
| **Named** | user / config | restart-safe | user only |
| **Sub** | LLM `agent_spawn` | session | creator |
| **Task** | LLM `agent_task` | one-shot | auto on return |

```
Main ──spawn──→ Named "pm" (in config)
                 └─spawn──→ Sub "analyst" (session-scoped)
                              ├─task──→ Task "search-jd"  ┐
                              └─task──→ Task "search-tb"  │ parallel
```

Each agent picks a backend independently: **Native Rust** (default, fastest), **Claude Code** (via Claude Agent SDK + ACP), **OpenCode** (FOSS coding agent), or **any ACP-compliant agent**. Toolset whitelist per agent: `minimal` (12) / `web` / `code` / `standard` (16) / `full`. Top-down delegation only — Sub can't call Main, siblings can't talk.

---

## Channels (13 + Custom)

| Channel | Protocol | Notes |
|---------|----------|-------|
| **WeChat Personal** | ilink long-poll | QR login; voice/image/file/video |
| **Feishu / Lark** | WebSocket | OAuth or appId+secret |
| **WeCom** | AI Bot WebSocket | |
| **QQ Bot** | Gateway WebSocket | |
| **DingTalk** | Stream Mode WS | |
| **Telegram** | HTTP long-poll | DM + group |
| **Discord** | Gateway WS | |
| **Slack** | Socket Mode | |
| **WhatsApp** | Cloud API webhook | |
| **Signal** | signal-cli JSON-RPC | |
| **LINE / Zalo** | Webhook | |
| **Matrix** | HTTP /sync | optional E2EE |
| **Custom** | `/hooks/{name}` | inbound webhook |

Every channel: DM/group ACL, pairing codes (8-char, 1 h), health monitoring, retry, streaming, file confirmation gates.

---

## LLM Providers (15+)

Qwen · DeepSeek · Kimi · Zhipu (GLM) · MiniMax · Doubao (ByteDance) · SiliconFlow · GateRouter · OpenRouter · Anthropic · OpenAI · Gemini · xAI (Grok) · Groq · Ollama · any OpenAI-compatible endpoint.

Features: failover chains, exponential backoff, model fallback (`primary` → `flash` → `vision` → `fallbacks`), thinking budget, Responses API, Ollama native, KV prefix-cache for RsClaw's own fleet (`rsclaw/*` namespace).

---

## Tools & Plugins

**36 built-in tools**: file read/write/search, shell exec (50+ safety deny patterns), web search/fetch/download, browser automation (CDP, 50+ actions, accessibility-tree snapshots), memory CRUD, knowledge-base CRUD, document extract/create (PDF / DOCX / XLSX / PPTX), image / video gen, voice STT (Whisper / SenseVoice), TTS, computer_use, cron jobs, multi-agent spawn/task, clarify (interactive Q&A), anycli (structured web extraction).

**40+ pre-parsed commands** (local, zero-token, sub-millisecond): `/run`, `/search`, `/help`, `/status`, `/clear`, `/compact`, `/ctx`, `/btw`, `/remember`, `/recall`, `/model`, `/cron`, `/plugin`, …

**Plugins** — dual-runtime by design:

| Runtime | Sandbox | When |
|---|---|---|
| **wasm** | runtime + `host.cli` allowlist | Untrusted, restricted hosts, mobile-portable |
| **node / bun / deno** | install-time allowlist gate | OpenClaw-compat, full system access |

`/plugin install <name>` from the slash command or desktop UI. Per-agent enablement caps prevent context-overflow (`tools_tokens` budget). Existing plugins: jimeng (image/video via Dreamina), douyin, wechat, xianyu, travel.

---

## Configuration

```json5
{
  gateway: { port: 18888 },
  models: {
    providers: {
      doubao:   { apiKey: "${DOUBAO_API_KEY}" },
      deepseek: { apiKey: "${DEEPSEEK_API_KEY}" },
      ollama:   { baseUrl: "http://localhost:11434" },
    },
  },
  agents: {
    defaults: {
      model: { primary: "doubao/doubao-seed-1-6-pro",
               flash: ["doubao/doubao-seed-2.0-lite"] },
    },
    list: [{ id: "main", default: true }],
  },
  channels: {
    telegram: { botToken: "${TELEGRAM_BOT_TOKEN}", dmPolicy: "pairing" },
  },
}
```

All strings support `${VAR}` env substitution. Precedence: CLI flag > `$RSCLAW_BASE_DIR/rsclaw.json5` > `~/.rsclaw/rsclaw.json5` > `./rsclaw.json5`.

---

## CLI cheatsheet

```bash
rsclaw setup / onboard / configure        # init + wizard
rsclaw start / stop / restart / status    # gateway control
rsclaw doctor --fix                       # health
rsclaw update / upgrade                   # self-update
rsclaw tools install chrome / ffmpeg / node / python / opencode
rsclaw channels login wechat              # QR scan
rsclaw memory status / search / docs      # memory ops
rsclaw knowledge ingest / search / list   # KB ops
rsclaw pairing pair / list / revoke       # channel pairing
rsclaw browser open / snapshot / click    # headless Chrome CLI
rsclaw anycli run / install / search      # structured web extraction
```

---

## Migrate from OpenClaw

```bash
openclaw gateway stop
rsclaw setup          # detects ~/.openclaw/, offers one-click import
rsclaw start
```

Import is read-only on `~/.openclaw/`; new data lands in `~/.rsclaw/`. Run both side-by-side on different ports (18888 vs 18789) — they don't share state.

| | RsClaw | OpenClaw |
|---|---|---|
| Binary size | ~15 MB single static | ~300 MB + node_modules |
| Startup | ~26 ms | 2–5 s |
| Idle memory | ~20 MB | ~1 GB |
| Long-term memory | three-tier + Weibull decay + hybrid | — |
| Knowledge base | OOXML + RRF retrieval, per-collection | — |
| A2A | Google v1.0 + hub-spoke relay | — |
| Browser | built-in CDP, 50+ actions | — |
| Multi-backend agents | Native / Claude Code / OpenCode / ACP | — |
| Exec safety | 50+ deny patterns | — |

---

## Development

```bash
git clone https://github.com/rsclaw-ai/rsclaw.git
cd rsclaw
cargo build --profile release-dev          # alias: `cargo brd`
RUST_LOG=rsclaw=debug cargo run -- gateway run
```

Requirements: Rust 1.91+, macOS / Linux / Windows. Optional: ffmpeg, Chrome.

```
src/
  agent/       runtime, memory, tools, preparse
  channel/     13 channel adapters
  config/      JSON5 loader, schema, env resolution
  gateway/     startup, hot reload, watch
  provider/    LLM providers, failover, prefix-cache
  server/      Axum HTTP, REST, OpenAI-compat
  store/       redb + tantivy + hnsw_rs
  browser/     Chrome CDP automation
  a2a/         A2A v1.0 + hub-spoke relay
  acp/         ACP client backends
  kb/          knowledge base + collections
  plugin/      wasm + node/bun/deno runtimes
  skill/       crystallization pipeline
```

---

## Support

- ⭐ **Star** — helps others find RsClaw
- 🐛 **Issues** — [github.com/rsclaw-ai/rsclaw/issues](https://github.com/rsclaw-ai/rsclaw/issues)
- 💬 **Community** — [WeChat / Feishu / QQ / Telegram](https://rsclaw.ai/en/community)
- 🤝 **Contribute** — [CONTRIBUTING.md](CONTRIBUTING.md)

## License

Dual-licensed under **MIT** ([LICENSE-MIT](LICENSE-MIT)) **OR** **Apache-2.0** ([LICENSE-APACHE](LICENSE-APACHE)) at your option.

Use freely in personal, commercial, enterprise, SaaS, or proprietary products. Modify and redistribute without copyleft obligations. Same license as Rust, Tokio, Serde, Axum.

Unless explicitly stated otherwise, contributions are dual-licensed under the same terms.

---

Built with 🦀 in Rust. Inspired by the OpenClaw community.
