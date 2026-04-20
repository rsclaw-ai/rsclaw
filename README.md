# RsClaw

> **An AI agent engine that remembers — and gets better the more you use it.**  
> One 15MB binary · 13 channels · 15 LLM providers · Multi-backend agents · OpenCLI-ready · Built in pure Rust.

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

Most AI agents forget everything between sessions. Every new conversation starts from zero — your preferences, your context, your workflow, all gone.

**RsClaw doesn't forget.**

Built from scratch in Rust, RsClaw (Crab AI / 螃蟹 AI) persists every interaction through a three-layer memory store (redb + tantivy + hnsw_rs), learns from your usage patterns, and ships as a single 15MB binary running on ~20MB RAM. Four agent lifetime modes (Main/Named/Sub/Task), four execution backends (Native Rust/Claude Code/OpenCode/ACP), 13 messaging channels, 15 LLM providers, A2A cross-machine orchestration — all without a line of Node.js. Drop-in OpenClaw replacement.

💬 [Join Community](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## Install

### 👉 New users

```bash
# macOS / Linux
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash

# Windows (PowerShell)
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

Then initialize:

```bash
rsclaw setup      # Initialize ~/.rsclaw/
rsclaw onboard    # Interactive wizard: provider, channels, etc.
rsclaw start
```

### 👉 Migrating from OpenClaw

```bash
openclaw gateway stop
rsclaw setup      # Detects OpenClaw data, offers one-click import
rsclaw start      # Everything just works — channels, agents, sessions
```

Your `~/.openclaw/` is never modified. See [Migrate from OpenClaw](#migrate-from-openclaw) below for details.

### Other install options

- **Desktop app** — `.dmg` / `.msi` / `.deb` from [Releases](https://github.com/rsclaw-ai/rsclaw/releases)
- **Via Cargo** — `cargo install rsclaw`
- **From source** — `git clone https://github.com/rsclaw-ai/rsclaw.git && cd rsclaw && cargo build --release`

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

### Built-in Tools (36)

File read/write/search, shell exec (with safety rules), web search/fetch/download, CDP browser automation (50+ actions), memory CRUD, document extraction/creation (PDF/DOCX/XLSX/PPTX), image/video generation, voice STT (Whisper/SenseVoice), TTS, computer_use, cron jobs (recurring + one-shot timer), multi-agent spawn/task, clarify (interactive Q&A), anycli (structured web data extraction).

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
- 50+ actions: open, snapshot, click, fill, scroll, screenshot, evaluate, annotate, capture_video, etc.
- Accessibility tree snapshots with `@e1` refs for LLM interaction
- Semantic locators: getbytext, getbyrole, getbylabel
- One-click video download: `rsclaw browser download-video <url>`
- Auth persistence: state save/load for login session reuse
- Memory-adaptive instance limits, 5-min idle timeout, crash auto-restart
- CLI: `rsclaw browser open/snapshot/click/screenshot/...` (full agent-browser parity)

### AnyCLI — Structured Web Data

Built-in [anycli](https://crates.io/crates/anycli) integration. Turn any website into structured CLI output with declarative YAML adapters:

```bash
rsclaw anycli run hackernews top --format table limit=10
rsclaw anycli run bilibili hot --format markdown
rsclaw anycli run github-trending repos language=rust
rsclaw anycli search zhihu        # search community hub
rsclaw anycli install zhihu       # install adapter
```

Built-in adapters: hackernews, bilibili, github-trending, arxiv, wikipedia. Community hub at [anycli.org](https://anycli.org). Agent uses `anycli` tool automatically when structured data is available — cleaner than web_fetch.

### Long-term Memory

Three-layer storage: redb (hot KV), tantivy (full-text search), hnsw_rs (vector similarity). Session compaction at 80% context window, `/compact` manual compression with memory save, `/clear` preserves conversation summary.

### Multi-Agent Architecture

Four agent types with up to 4-layer delegation:

| Type | Created by | Lifetime | Persisted |
|------|-----------|----------|-----------|
| **Main** | System | Forever | Config (`default: true`) |
| **Named** | User | Permanent | Config file (survives restart) |
| **Sub** | LLM | Session | Memory only (gone on restart) |
| **Task** | LLM | One-shot | Auto-destroyed after completion |

```
Main ──spawn──→ Named "pm" (persistent, in config)
                 └─spawn──→ Sub "analyst" (temporary)
                              ├─task──→ Task "search-jd" (parallel)
                              └─task──→ Task "search-tb" (parallel)
```

Each agent can use a different execution backend:

| Backend | Description |
|---------|-------------|
| **Native Rust** | Built-in LLM runtime (default, fastest) |
| **Claude Code** | Claude Agent SDK via ACP protocol |
| **OpenCode** | Open-source coding agent |
| **ACP** | Any Agent Client Protocol compliant agent |

```json5
{
  agents: {
    list: [
      { id: "main", default: true, model: { primary: "qwen-plus" } },
      { id: "coder", model: { primary: "deepseek-chat", toolset: "code" },
        claudecode: { command: "claude-agent-acp" } },  // uses Claude Code backend
    ],
    external: [
      { id: "gpu-worker", url: "http://gpu-server:18888", token: "${TOKEN}" },
    ],
  },
}
```

Collaboration modes: sequential (chain), parallel (fan-out), orchestrated (LLM-driven `agent_<id>` tool calls).

Permission model:
- **Toolset** per agent: `minimal` (12 tools) / `web` / `code` / `standard` (16) / `full` (all)
- **Exec safety**: 50+ global deny patterns apply to ALL agents (cannot be bypassed)
- **Main cannot be killed**; Named/Sub/Task can be killed by their creator
- Agents cannot communicate upward or sideways — delegation is strictly top-down

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

### What you gain

| | RsClaw | OpenClaw |
|---|---|---|
| **Binary size** | ~15MB | ~300MB+ (node_modules) |
| **Startup** | ~26ms | 2–5s |
| **Idle memory** | ~20MB | ~1000MB+ |
| **Long-term memory** | Three-layer (redb + tantivy + hnsw_rs) | — |
| **Self-learning** | Learns from your usage patterns | — |
| **Multi-backend agents** | Native Rust / Claude Code / OpenCode / ACP | — |
| **A2A cross-machine** | Google A2A v0.3 | — |
| **Browser automation** | Built-in headless Chrome (CDP) | — |
| **Exec safety** | 50+ deny patterns, deny/confirm/allow | — |

### FAQ

**Can I run RsClaw and OpenClaw simultaneously?**  
Yes. RsClaw uses port 18888 by default, OpenClaw uses 18789. They have separate data directories (`~/.rsclaw/` vs `~/.openclaw/`) and can run side by side without conflict.

**Will RsClaw modify my OpenClaw data?**  
Never. Import mode is strictly read-only on `~/.openclaw/`. All RsClaw data goes to `~/.rsclaw/`.

**How do I switch back to OpenClaw?**  
`rsclaw stop && openclaw gateway start`. Your `~/.openclaw/` is untouched.

**Does it work offline?**  
Yes — with Ollama for local models, or any OpenAI-compatible local endpoint. Voice STT can also run fully local via Candle Whisper.

**Can I use RsClaw in my commercial product?**  
Yes, freely. RsClaw is dual-licensed under MIT OR Apache-2.0 — you can build proprietary products, run SaaS services, or redistribute modified versions without open-source obligations.

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

## Support the project

If RsClaw saves you hours, consider:

- ⭐ **Star this repo** — helps other developers discover RsClaw
- 🐛 **Report bugs** via [GitHub Issues](https://github.com/rsclaw-ai/rsclaw/issues)
- 💬 **Join the community** — [WeChat / Feishu / QQ / Telegram](https://rsclaw.ai/en/community)
- 🤝 **Contribute** — see [CONTRIBUTING.md](CONTRIBUTING.md)

## License

Licensed under either of

- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- **MIT license** ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### What this means

- ✅ **Use freely** in personal, commercial, and enterprise projects
- ✅ **Modify and redistribute** without any obligation to open-source your changes
- ✅ **Build proprietary products** on top of RsClaw
- ✅ **Run as a SaaS service** without any licensing requirements
- ✅ **No copyleft** — your derivative work stays yours

This is the same dual-license used by the Rust language itself, Tokio, Serde, Axum, and most of the Rust ecosystem.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

---

Built with 🦀 in Rust. Inspired by the OpenClaw community.
