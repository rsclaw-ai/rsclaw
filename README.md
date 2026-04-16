# RsClaw

**Your AI Automation Butler — one binary, 13 channels, 15 LLM providers, long-term memory, browser automation, all in Rust.**

[![Rust](https://img.shields.io/badge/Rust-1.91%20Edition%202024-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-AGPL--3.0-blue)](LICENSE)
[![Binary Size](https://img.shields.io/badge/binary-~15MB-green)]()

**English** | [中文](docs/lang/README_cn.md) | [日本語](docs/lang/README_ja.md) | [한국어](docs/lang/README_ko.md) | [ไทย](docs/lang/README_th.md) | [Tiếng Việt](docs/lang/README_vi.md) | [Français](docs/lang/README_fr.md) | [Deutsch](docs/lang/README_de.md) | [Español](docs/lang/README_es.md) | [Русский](docs/lang/README_ru.md)

RsClaw (Crab AI / 螃蟹AI自动化管家) is your personal AI butler — a single 15MB binary that connects all your messaging apps to AI agents. It manages your tasks, browses the web, remembers conversations, and runs 24/7 on your desktop or server. Built from scratch in Rust with one-click OpenClaw migration.

<p align="center">
  <img src="docs/images/en.gif" alt="RsClaw Preview" width="800" />
</p>

💬 [Join Community](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## Install

```bash
# macOS / Linux
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash

# Windows (PowerShell)
irm https://app.rsclaw.ai/scripts/install.ps1 | iex

# From source
git clone https://github.com/rsclaw-ai/rsclaw.git && cd rsclaw
cargo build --release
```

Desktop app (.dmg / .msi / .deb): [Releases](https://github.com/rsclaw-ai/rsclaw/releases)

```bash
# Migrate from OpenClaw
rsclaw setup      # detects OpenClaw data, offers import
rsclaw start      # config already imported, ready to go

# New install
rsclaw setup      # initialize ~/.rsclaw/
rsclaw onboard    # interactive wizard: provider, channels, etc.
rsclaw start
```

---

## Channels (13 + Custom)

| Channel | Protocol |
|---------|----------|
| **WeChat Personal** | ilink long-poll (QR scan login, voice/image/file/video) |
| **Feishu / Lark** | WebSocket (OAuth or appId + appSecret) |
| **WeCom** | AI Bot WebSocket |
| **QQ Bot** | WebSocket Gateway |
| **DingTalk** | Stream Mode WebSocket |
| **Telegram** | HTTP long-poll (DM + group, voice/image/file/video) |
| **Discord** | Gateway WebSocket |
| **Slack** | Socket Mode WebSocket |
| **WhatsApp** | Cloud API Webhook |
| **Signal** | signal-cli JSON-RPC |
| **LINE** | Webhook |
| **Zalo** | Webhook |
| **Matrix** | HTTP /sync (optional E2EE) |
| **Custom** | Webhook POST to `/hooks/{name}` |

All channels support: DM/group policy (open/pairing/allowlist), health monitoring, message retry, pairing codes (8-char, 1hr TTL), streaming modes, file upload confirmation.

---

## LLM Providers (15+)

Qwen, DeepSeek, Kimi, Zhipu (GLM), MiniMax, Doubao (ByteDance), SiliconFlow, GateRouter, OpenRouter, Anthropic, OpenAI, Gemini, xAI (Grok), Groq, Ollama, or any OpenAI-compatible API.

Features: failover with exponential backoff, model fallback chains, thinking budget, Responses API + completions + Ollama native.

---

## Key Features

### Built-in Tools (32)

File read/write, shell exec (with safety rules), web search/fetch, CDP browser automation (20 actions), memory CRUD, document extraction (PDF/DOCX/XLSX/PPTX), image compression, voice STT (Whisper/SenseVoice), TTS, computer_use, cron jobs, multi-agent spawn.

### Pre-parsed Commands (40+)

Local commands that bypass the LLM — zero token cost, sub-millisecond:

```
/run <cmd>    Shell exec (pipes, redirects)     /search <q>   Web search
/help         Show commands                      /status       Gateway status
/clear        Clear session                      /compact      Compress + save memory
/ctx <text>   Add session context                /btw <q>      Side-channel quick query
/remember     Save to long-term memory           /recall       Search memory
/model <name> Switch model                       /cron list    List cron jobs
```

### Browser Automation (CDP)

Built-in headless Chrome — no ChromeDriver, no Playwright, no Node.js:
- 20 actions: open, snapshot, click, fill, scroll, screenshot, evaluate, etc.
- Accessibility tree snapshots with `@e1` refs for LLM interaction
- Memory-adaptive instance limits, 5-min idle timeout, crash auto-restart

### Long-term Memory

Three-layer storage: redb (hot KV), tantivy (full-text search), hnsw_rs (vector similarity). Session compaction at 80% context window, `/compact` manual compression with memory save, `/clear` preserves conversation summary.

### Multi-Agent

```json5
{
  agents: {
    list: [
      { id: "main", model: { primary: "anthropic/claude-sonnet-4-5" }, allowed_commands: "*" },
      { id: "coder", model: { primary: "deepseek/deepseek-chat" }, allowed_commands: "read|write|exec" },
    ],
    external: [
      { id: "remote", url: "https://remote-gateway.example.com", auth_token: "${TOKEN}" },
    ],
  },
}
```

Collaboration modes: sequential (chain), parallel (fan-out), orchestrated (LLM-driven tool calls).

### A2A Protocol

Implements [Google A2A v0.3](https://a2a-protocol.org/) for cross-network agent collaboration. Auto-discovery via `/.well-known/agent.json`, JSON-RPC 2.0 task dispatch, streaming support.

### Security

- **Exec safety**: 50+ deny patterns (sudo, rm -rf /, .ssh, .env, etc.), configurable deny/confirm/allow
- **Write sandbox**: path isolation + content scanning
- **File upload**: two-layer confirmation (size gate + token gate)
- **Per-agent permissions**: configurable command ACL
- **Tool loop detection**: sliding window (12-call, 8-threshold)

---

## Configuration

```json5
{
  gateway: { port: 18888 },
  models: {
    providers: {
      qwen: { apiKey: "${DASHSCOPE_API_KEY}" },
      ollama: { baseUrl: "http://localhost:11434" },
    },
  },
  agents: {
    defaults: {
      model: { primary: "qwen/qwen-turbo" },
      timeoutSeconds: 600,
    },
  },
  channels: {
    telegram: { botToken: "${TELEGRAM_BOT_TOKEN}", dmPolicy: "pairing" },
    feishu: { appId: "cli_xxx", appSecret: "${FEISHU_APP_SECRET}" },
  },
}
```

All string values support `${VAR}` env substitution. Config priority: CLI flag > `$RSCLAW_BASE_DIR/rsclaw.json5` > `~/.rsclaw/rsclaw.json5` > `./rsclaw.json5`.

---

## CLI

```bash
rsclaw setup                       # First-time setup wizard
rsclaw start / stop / restart      # Gateway control
rsclaw status                      # Check status
rsclaw doctor --fix                # Health check
rsclaw configure                   # Interactive config (7 sections)
rsclaw update                      # Auto-update from GitHub
rsclaw tools install chrome        # Install tools (chrome/ffmpeg/node/python/opencode)
rsclaw tools status                # Check tool availability
rsclaw pairing pair / list / revoke
rsclaw channels login wechat       # QR scan login
```

---

## Migrate from OpenClaw

```bash
openclaw gateway stop
rsclaw setup          # detects OpenClaw, offers import
rsclaw start
```

Import copies config, workspace, and sessions into `~/.rsclaw/`. OpenClaw data is never modified. All OpenClaw config fields are supported.

| Feature | RsClaw | OpenClaw |
|---------|--------|----------|
| Language | Rust | TypeScript |
| Binary | ~15MB | ~300MB+ (node_modules) |
| Startup | ~26ms | 2-5s |
| Memory | ~20MB idle | ~1000MB+ |
| Channels | 13 + custom | 8 |
| A2A protocol | v0.3 | -- |
| Browser | built-in CDP | -- |
| Exec safety | deny/confirm/allow | -- |

---

## Development

```bash
cargo test
RUST_LOG=rsclaw=debug cargo run -- gateway run

# Cross-compile
./scripts/build.sh all     # all platforms
./scripts/build.sh macos   # macOS x86_64 + ARM64
./scripts/build.sh linux   # Linux musl static
./scripts/build.sh windows # Windows MSVC
```

### Architecture

```
src/
  agent/       Agent runtime, memory, tools, preparse
  channel/     13 channels
  config/      JSON5 loader, schema
  gateway/     Startup, hot reload
  provider/    LLM: Anthropic, OpenAI, Gemini, Ollama, failover
  server/      Axum HTTP, REST API, OpenAI-compat endpoints
  store/       redb + tantivy + hnsw_rs
  browser/     Chrome CDP automation
  a2a/         Google A2A v0.3
  acp/         ACP protocol
  ws/          WebSocket v3
```

Requirements: Rust 1.91+, macOS / Linux / Windows. Optional: ffmpeg, Chrome.

---

## License

[AGPL-3.0](LICENSE) — Free to use, modify, and distribute. Network services must open-source modifications under the same license.
