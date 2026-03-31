# RsClaw

**A high-performance, multi-agent AI ecosystem with seamless OpenClaw compatibility.**

[![Rust](https://img.shields.io/badge/Rust-1.91%20Edition%202024-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-MIT%2FApache--2.0-blue)](LICENSE-MIT)
[![Binary Size](https://img.shields.io/badge/binary-~17MB-green)]()

**English** | [中文](README_zh.md)

RsClaw is a ground-up Rust rewrite of [OpenClaw](https://github.com/openclaw/openclaw), delivering the same multi-agent AI gateway protocol with 10x faster startup, 10x smaller footprint, and zero Node.js dependency. It reads your existing OpenClaw configuration directory unchanged -- stop OpenClaw, start RsClaw, and every channel, agent, and model provider continues working immediately.

---

## Migrate from OpenClaw

```bash
# Stop OpenClaw
openclaw gateway stop

# Setup RsClaw (detects OpenClaw data, offers import)
rsclaw setup

# Start RsClaw
rsclaw gateway start
```

`rsclaw setup` detects your existing OpenClaw installation and offers two options:

- **Import** (recommended) -- copies config, workspace, and session history into `~/.rsclaw/`. OpenClaw data is read-only, never modified.
- **Fresh** -- starts clean, ignores OpenClaw data.

Config resolution order:

| Priority | Source |
|----------|--------|
| 1 (highest) | `--config-path <file>` CLI flag |
| 2 | `$RSCLAW_BASE_DIR/rsclaw.json5` |
| 3 | `~/.rsclaw/rsclaw.json5` |
| 4 (lowest) | `.rsclaw.json5` (current directory) |

All OpenClaw config fields are supported. Unknown fields are silently ignored for forward compatibility.

---

## RsClaw vs OpenClaw

| Feature | RsClaw | OpenClaw |
|---------|--------|----------|
| Language | Rust | TypeScript/Node.js |
| Binary size | ~17MB | ~300MB+ (node_modules) |
| Startup time | <100ms | 2-5s |
| Memory usage | ~20MB idle | ~1000MB+ |
| Dependencies | 542 (Rust crates) | 1000+ (npm) |
| Protocol compat | OpenClaw WS v3 (full) | Native |
| OpenAI compat | `/v1/chat/completions` + `/v1/models` | `/v1/chat/completions` |
| Channels | 13 + custom webhook | 8 |
| LLM providers | 15 pre-configured | ~10 |
| Built-in tools | 32 | ~25 |
| Pre-parsed commands | 40+ (zero token, <1ms) | -- |
| Shell integration | full `sh -c` (pipes, redirects) | -- |
| CDP browser | built-in headless Chrome (20 actions) | -- |
| Read/write safety | blocks .ssh, .env, credentials | -- |
| Customizable defaults | runtime defaults.toml override | -- |
| Exec safety rules | deny/confirm/allow (40+ patterns) | -- |
| Write sandbox | path isolation + content scan | -- |
| File upload gate | two-layer confirmation (size + token) | -- |
| Vision auto-detect | model name pattern matching | -- |
| Image compression | auto-resize to 1024px JPEG | -- |
| Office doc extraction | DOCX/XLSX/PPTX (native, no external tools) | -- |
| Per-agent permissions | configurable command ACL | -- |
| Tool loop detection | sliding window (12-call/8-threshold) | -- |
| Upload runtime tuning | /set_upload_size, /set_upload_chars | -- |
| Skill registries | ClawHub + SkillHub (auto fallback) | ClawHub only |
| computer_use | native screenshot/mouse/keyboard | via browser only |
| Config format | JSON5 | JSON5 |
| Hot reload | auto-restart on channel changes | Yes |
| Self-update | `rsclaw update` from GitHub | npm update |

---

## RsClaw-Exclusive Features

### Pre-parsed Commands (40+)

Local commands that bypass the LLM entirely -- zero token cost, sub-millisecond response.

**Shell / Exec** -- full shell support with pipes, redirects, and chaining:

| Command | Description |
|---------|-------------|
| `/run <cmd>` | Execute any shell command via `sh -c` (supports pipes: `ls \| grep rs`) |
| `$ <cmd>` | Shell shortcut (same as /run) |
| `! <cmd>` | Shell shortcut (same as /run) |
| `/ls [args]` | List files (behaves like native `ls`, e.g. `/ls -la src/`) |
| `/cat <file>` | Read file content |
| `/read <file>` | Read file content (alias for /cat) |
| `/write <path> <content>` | Write content to a file |
| `/find <pattern>` | Find files by name (`find . -name <pattern>`) |
| `/grep <pattern>` | Search file contents (`grep -rn <pattern>`) |

**Web & Search:**

| Command | Description |
|---------|-------------|
| `/search <query>` | Web search (DuckDuckGo/Google/Bing) |
| `/google <query>` | Web search (alias) |
| `/fetch <url>` | Fetch and extract web page content |
| `/screenshot <url>` | Take a screenshot of a web page |
| `/ss` | Take a desktop screenshot |

**System & Session:**

| Command | Description |
|---------|-------------|
| `/help` | Show all available commands |
| `/version` | Show version (date + git hash) |
| `/status` | Gateway status |
| `/health` | Health check |
| `/uptime` | Show uptime |
| `/models` | List available models |
| `/model <name>` | Switch primary model |
| `/clear` | Clear current session |
| `/reset` | Reset session |
| `/history [n]` | Show last N messages (default 20) |
| `/sessions` | List all sessions |
| `/cron list` | List scheduled cron jobs |
| `/send <to> <msg>` | Send message to a channel/user |

**Memory:**

| Command | Description |
|---------|-------------|
| `/remember <text>` | Save to long-term memory |
| `/recall <query>` | Search memory |

**Upload Limits:**

| Command | Description |
|---------|-------------|
| `/get_upload_size` | Show current file size limit |
| `/set_upload_size <MB>` | Set file size limit (runtime, resets on restart) |
| `/get_upload_chars` | Show current text char limit |
| `/set_upload_chars <n>` | Set text char limit (runtime, resets on restart) |
| `/config_upload_size <MB>` | Set file size limit (saved to config file) |
| `/config_upload_chars <n>` | Set text char limit (saved to config file) |

**Chinese Natural Language:**

| Command | Description |
|---------|-------------|
| `搜索 <query>` | Web search |
| `截屏` / `截图` | Desktop screenshot |
| `查天气 <city>` | Weather search |
| `几点了` | Current time |
| `今天几号` | Current date |
| `计算 <expr>` | Calculate expression |
| `查IP` | Show public IP address |

### Exec Safety Rules

Configurable deny/confirm/allow patterns that protect against dangerous operations. 50+ built-in deny patterns:

- **Deny**: `sudo`, `rm -rf /`, `dd`, `mkfs`, `shutdown`, `curl|sh`, read/write `.ssh/`, `.env`, `openclaw.json`, `rsclaw.json5`, etc.
- **Confirm**: `rm -rf`, `git push --force`, `git reset --hard`, `docker rm`, `drop database`, etc.
- **Allow**: whitelist to override deny rules

Read protection blocks access to: SSH keys, GPG keys, cloud credentials (`.aws/`, `.kube/`, `.gcloud/`), AI tool configs (`.claude/`, `.opencode/`, `openclaw.json`, `rsclaw.json5`), shell history, database passwords, and system auth files.

Enable with `tools.exec.safety = true` in config.

### Two-Layer File Upload Confirmation

Prevents accidental token waste from large files:

- **Layer 1 (Size Gate)**: File > 50MB triggers confirmation with options: analyze / save to workspace / discard
- **Layer 2 (Token Gate)**: Extracted text > 50,000 chars triggers token cost confirmation

Limits adjustable at runtime via `/set_upload_size` and `/set_upload_chars`.

### Vision Auto-Detection

Automatically detects whether the current model supports images (GPT-4V, Claude 3, Gemini, Qwen-VL, etc.). Non-vision models receive `[image]` text placeholders instead of base64 data, preventing silent token waste.

### Office Document Extraction

DOCX, XLSX, and PPTX files are extracted to plain text natively using the zip crate -- no external tools required. Integrated into the file upload flow.

### Image Compression

Images are automatically resized to 1024px max dimension and converted to JPEG before sending to the LLM, reducing token consumption.

### Write Sandbox

Workspace path isolation and content scanning. Blocks writes to sensitive system paths and scans script content for dangerous patterns.

### Per-Agent Command Permissions

Main agent gets `*` (all commands). Other agents are restricted by configuration, preventing unauthorized tool access.

### Tool Loop Detection

Sliding-window detector (12-call window, 8-call threshold) prevents infinite tool call loops. Automatically resets after productive operations.

### Configure Section Menu

Interactive `rsclaw configure` with 7 sections:

1. Gateway (port, bind address)
2. Model Provider (provider, API key, model)
3. Channels (add/remove/configure one at a time)
4. Web Search (provider, API keys)
5. Upload Limits (file size, text chars, vision toggle)
6. Exec Safety (on/off)

Supports `--section` flag for direct access: `rsclaw configure --section channels`.

### CDP Browser Automation

Built-in headless Chrome control via Chrome DevTools Protocol -- no ChromeDriver, no Playwright, no Node.js:

- **20 actions**: open, snapshot, click, fill, type, select, check/uncheck, scroll, screenshot, pdf, back, forward, reload, get_text, get_url, get_title, wait, evaluate, cookies
- **Accessibility tree snapshots** with `@e1`, `@e2` element references for LLM-friendly interaction
- **Memory-adaptive**: auto-limits Chrome instances based on system RAM (1 per 2GB, min 200MB free)
- **Auto-lifecycle**: 5-minute idle timeout, crash detection + auto-restart, process cleanup on drop
- **Zero dependency**: uses Chrome/Chromium directly, no extra drivers needed

### Customizable Defaults (`defaults.toml`)

Place a `defaults.toml` in `$base_dir/` to override the built-in defaults at runtime -- no recompilation needed:

- Provider definitions (add/remove LLM providers)
- Channel field definitions (customize onboard/configure wizard)
- Exec safety rules (deny/confirm/allow patterns)
- Search engine URLs
- Skill registry URLs

`rsclaw setup` writes a copy for you to edit. External file takes priority; built-in version is the fallback.

### Additional Exclusives

- **Dual Skill Registry** -- ClawHub + SkillHub (Tencent COS) with automatic fallback
- **computer_use Tool** -- native desktop screenshots, mouse and keyboard control
- **ACP extended commands** -- spawn/connect/run/send/list/kill (OpenClaw only has `client`)
- **Pairing revoke** -- OpenClaw only has approve + list
- **`--base-dir` / `--config-path` global flags** -- flexible config path override
- **Date-based versioning** -- automatic `YYYY.M.D (git-hash)` from build date + git commit

---

## Quick Install

### Pre-built Binaries (Recommended)

```bash
# macOS / Linux (auto-detect platform)
curl -fsSL https://raw.githubusercontent.com/rsclaw-ai/rsclaw/main/scripts/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/rsclaw-ai/rsclaw/main/scripts/install.ps1 | iex
```

Supported platforms: macOS (x86_64, ARM64), Linux (x86_64, ARM64), Windows (x86_64, ARM64).

### From Source

```bash
git clone https://github.com/rsclaw-ai/rsclaw.git
cd rsclaw
cargo build --release
# Binary at ./target/release/rsclaw (~17MB)
```

### Local Cross-Compilation

```bash
# Build for all platforms from macOS/Linux host
./scripts/build.sh all

# Or specific platform groups
./scripts/build.sh macos    # macOS x86_64 + ARM64
./scripts/build.sh linux    # Linux x86_64 + ARM64 (musl, static)
./scripts/build.sh windows  # Windows x86_64 (MSVC via cargo-xwin)
```

### First Run

```bash
# First-time setup (detects OpenClaw data if present)
rsclaw setup

# Start gateway
rsclaw gateway start
```

---

## Quick Start

```bash
# Interactive setup wizard
rsclaw onboard

# Start gateway
rsclaw gateway start

# Check status
rsclaw status

# Health check
rsclaw doctor --fix

# Configure (section-based menu)
rsclaw configure

# Configure specific section
rsclaw configure --section channels
```

---

## Update / Upgrade

```bash
# Auto-update from GitHub release
rsclaw update

# Or manually from source
cd /path/to/rsclaw
git pull origin main
cargo build --release

# Check current version
rsclaw --version
```

`rsclaw update` downloads the latest release binary from [github.com/rsclaw-ai/rsclaw](https://github.com/rsclaw-ai/rsclaw) and replaces the current binary in-place. The gateway auto-restarts after update if running as a service.

---

## Supported Channels (13 + Custom)

| # | Channel | Protocol | Setup |
|---|---------|----------|-------|
| 1 | **WeChat Personal** | ilink long-poll | QR scan via `rsclaw channels login wechat`. Voice STT, image/file/video, SILK decode. |
| 2 | **Feishu / Lark** | WebSocket | OAuth scan or manual `appId` + `appSecret`. Event dedup, rich text. |
| 3 | **WeCom** | AI Bot WebSocket | `botId` + `secret` (企业微信后台). Auto-reconnect, markdown replies. |
| 4 | **QQ Bot** | WebSocket Gateway | `appId` + `appSecret`. Group/C2C/Guild support, sandbox mode. |
| 5 | **DingTalk** | Stream Mode WebSocket | `appKey` + `appSecret`. DM + group, voice transcription. |
| 6 | **Telegram** | HTTP long-poll | `botToken`. DM + group (@mention), voice/image/file/video, inline images. |
| 7 | **Matrix** | HTTP /sync long-poll | `homeserver` + `accessToken` + `userId`. Optional E2EE (`--features channel-matrix`). |
| 8 | **Discord** | Gateway WebSocket | Bot token. Guild/DM, reaction notifications, streaming edits. |
| 9 | **Slack** | Socket Mode WebSocket | `botToken` + `appToken`. No public URL needed. |
| 10 | **WhatsApp** | Webhook (Cloud API) | `WHATSAPP_PHONE_NUMBER_ID` + `WHATSAPP_ACCESS_TOKEN` env vars. Meta webhook verification. |
| 11 | **Signal** | signal-cli JSON-RPC | Phone number + signal-cli binary. Encrypted messaging. |
| 12 | **LINE** | Webhook | `channelAccessToken` + `channelSecret`. Push/Reply API. |
| 13 | **Zalo** | Webhook | `accessToken` + `oaSecret`. Official Account API. |
| -- | **Custom Webhook** | Webhook POST | Send JSON to `/hooks/{name}`. Generic inbound handler for any platform. |

Channel features: DM/Group policy (open/pairing/allowlist/disabled), health monitoring, text chunking with code-fence protection, message retry with exponential backoff, pairing codes (6-char, 1-hour TTL), streaming modes (off/partial/block/progress), file upload two-layer confirmation.

---

## LLM Providers (15 Pre-configured)

| Provider | Base URL | Notes |
|----------|----------|-------|
| **Qwen** (Alibaba DashScope) | dashscope.aliyuncs.com | qwen-turbo, qwen-plus, qwen-max |
| **DeepSeek** | api.deepseek.com | Streaming tool call accumulation |
| **Kimi** (Moonshot) | api.moonshot.cn | |
| **Zhipu** (GLM) | open.bigmodel.cn | |
| **MiniMax** | api.minimax.chat | |
| **GateRouter** | api.gaterouter.com | Multi-model routing |
| **OpenRouter** | openrouter.ai/api | |
| **Anthropic** | api.anthropic.com | Claude 3/4 family |
| **OpenAI** | api.openai.com | GPT-4o, o1, o3 |
| **Google Gemini** | generativelanguage.googleapis.com | |
| **xAI** (Grok) | api.x.ai | |
| **Groq** | api.groq.com | Llama, Mixtral |
| **SiliconFlow** | api.siliconflow.cn | |
| **Ollama** | localhost:11434 | Local models |
| **Custom** | user-defined | Any OpenAI-compatible API |

Provider features: failover with exponential backoff, model fallback chains, image fallback models, thinking budget allocation, token usage tracking, auto-registration from config/env/auth-profiles.

---

## Built-in Tools (32)

| Category | Tools |
|----------|-------|
| **File** | `read`, `write` |
| **Shell** | `exec` (with safety rules) |
| **Memory** | `memory_search`, `memory_get`, `memory_put`, `memory_delete` |
| **Web** | `web_search`, `web_fetch`, `web_browser`, `computer_use` |
| **Media** | `image`, `pdf`, `tts` |
| **Messaging** | `message`, `telegram_actions`, `discord_actions`, `slack_actions`, `whatsapp_actions`, `feishu_actions`, `weixin_actions`, `qq_actions`, `dingtalk_actions` |
| **Session** | `sessions_send`, `sessions_list`, `sessions_history`, `session_status` |
| **System** | `cron`, `gateway`, `subagents`, `agent_spawn`, `agent_list` |

Web search engines: DuckDuckGo (default), Brave, Google, Bing -- configurable via `rsclaw configure --section web_search`.

---

## Storage Architecture

| Layer | Engine | Purpose |
|-------|--------|---------|
| **Hot KV** | redb 2 | Sessions, messages, pairing state, config cache |
| **Full-Text Search** | tantivy 0.22 | Memory search, document indexing |
| **Vector Search** | hnsw_rs 0.3 | Semantic similarity, auto-recall |

Data stored in `$base_dir/var/` -- `var/data/` (redb/search/memory), `var/run/`, `var/logs/`, `var/cache/`.

---

## Configuration

Example `rsclaw.json5`:

```json5
{
  gateway: {
    port: 18789,
    bind: "loopback",
  },
  models: {
    providers: {
      qwen: {
        apiKey: "${DASHSCOPE_API_KEY}",
        baseUrl: "https://dashscope.aliyuncs.com/compatible-mode/v1",
      },
    },
  },
  agents: {
    defaults: {
      model: { primary: "qwen/qwen-turbo" },
      thinking: { level: "medium" },
    },
  },
  channels: {
    telegram: { botToken: "${TELEGRAM_BOT_TOKEN}" },
    feishu: { appId: "xxx", appSecret: "xxx" },
  },
  tools: {
    exec: { safety: true },
    upload: { max_file_size: 50000000, max_text_chars: 50000 },
  },
}
```

### Provider Auto-Registration

LLM providers are auto-registered from:
1. Config `models.providers` section
2. Environment variables (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, etc.)

### Multi-Instance

```bash
rsclaw --dev gateway run          # Uses ~/.rsclaw-dev
rsclaw --profile test gateway run # Uses ~/.rsclaw-test
```

---

## Integrations

### MCP (Model Context Protocol)

Spawn MCP server subprocesses with JSON-RPC tool discovery. Tools are auto-registered as `mcp_<server>_<tool>`. Configure in `mcp` config section.

### Plugins

Hook-based plugin architecture with lifecycle events: `pre_turn`, `post_turn`, `pre_tool_call`, `post_tool_call`, `on_error`. Plugins loaded from `plugins/` directory.

### Skills

External skill packages from ClawHub and SkillHub registries. Install via `rsclaw skills install <name>` or `/skill install <name>`.

### External Agents

Remote agent invocation via HTTP. Tools auto-registered as `agent_<id>`. A2A (Agent-to-Agent) protocol with Bearer token auth.

### Cron Jobs

Schedule agents to run periodically with cron expressions. Manage via `rsclaw cron` or `/cron list`.

### Webhooks

Webhook ingress at `/hooks/:path` with action dispatch (call agent, trigger cron, etc.).

---

## Roadmap

### Phase 1 -- CLI Parity + Stability

- Existing commands: add --json/--verbose/--timeout common options
- New commands: completion, dashboard, daemon, qr, docs, uninstall
- Medium commands: agent (singular), devices, directory, approvals
- Gateway/doctor/logs/sessions/status option gaps
- Control UI: remaining 5 WS API methods + config schema pages

### Phase 2 -- Large Commands + Ecosystem

- message command tree (25+ subcommands)
- node/nodes distributed computing commands
- onboard 70+ non-interactive flags
- Plugin marketplace + uninstall/update/inspect

### Phase 3 -- Advanced Features

- browser command (35+ CDP subcommands)
- `--container` global option (Podman/Docker)
- Video frame extraction for non-Gemini models
- WeCom/Signal multimedia sending

### Phase 4 -- Public Release

- 100% CLI compatibility (excluding browser)
- 100% Control UI compatibility
- Homebrew / cargo install distribution
- Complete documentation site

---

## FAQ

**Can I run RsClaw and OpenClaw simultaneously?**
Yes. RsClaw defaults to port 18888, OpenClaw defaults to 18789. They use separate data directories (`~/.rsclaw/` vs `~/.openclaw/`) and can run side by side.

**Will RsClaw modify my OpenClaw data?**
Never. Import mode reads OpenClaw files (config, workspace, sessions) but never writes to `~/.openclaw/`. All rsclaw data goes to `~/.rsclaw/`.

**How do I switch back to OpenClaw?**
`rsclaw gateway stop && openclaw gateway start`. Your `~/.openclaw/` is untouched.

**Does it support all OpenClaw WebSocket methods?**
33+ methods implemented including chat streaming. RsClaw is wire-compatible with the OpenClaw WebUI (Control Panel) at `http://localhost:18789`.

**What about Node.js skills/plugins?**
RsClaw can install and run skills from ClawHub and SkillHub. Node.js runtime is needed for JS-based skills.

**How do I enable exec safety?**
Set `tools.exec.safety = true` in config, or use `rsclaw configure --section exec_safety`. 40+ deny patterns are built-in. Customize in `defaults.toml`.

**How do I update RsClaw?**
Run `rsclaw update` to download the latest release from GitHub. For source builds, `git pull && cargo build --release`.

**Where does RsClaw store data?**
In `~/.rsclaw/`. Import mode copies OpenClaw data here during setup. RsClaw and OpenClaw directories are completely separate.

**How do I configure file upload limits?**
Use `rsclaw configure --section upload_limits` or set `tools.upload.max_file_size` / `tools.upload.max_text_chars` in config. Runtime adjustable via `/set_upload_size` and `/set_upload_chars`.

---

## Development

```bash
# Run tests
cargo test

# Run with debug logging
RUST_LOG=rsclaw=debug cargo run -- gateway run

# Build release
cargo build --release
```

### Architecture

```
src/
  agent/       # Agent runtime, memory, tool dispatch, loop detection, preparse
  channel/     # 13 channels: Telegram, WeChat, Feishu, DingTalk, etc.
  config/      # JSON5 loader, schema, 6-level config priority
  gateway/     # Startup, hot reload, channel wiring
  mcp/         # MCP client (JSON-RPC over stdin/stdout)
  plugin/      # Plugin shell bridge, hook registry
  provider/    # LLM providers: Anthropic, OpenAI, Gemini, failover
  server/      # Axum HTTP server, REST API, OpenAI-compat endpoints
  skill/       # Skill loader, ClawHub/SkillHub client, tool runner
  store/       # redb KV + tantivy BM25 + hnsw_rs vector
  ws/          # WebSocket protocol v3
  cmd/         # CLI commands: setup, configure, security, etc.
  acp/         # ACP protocol (agent spawn/connect/run)
```

### Matrix E2EE

Build with `cargo build --release --features channel-matrix` for encrypted room support. Requires a recovery key in config (`recoveryKey` field under `channels.matrix`). Without the feature flag, Matrix uses a lightweight reqwest-based driver (unencrypted rooms only).

### Requirements

- Rust 1.91+ (Edition 2024)
- macOS / Linux / Windows
- Optional: ffmpeg (image compression, voice transcription)
- Optional: whisper-cpp (local STT)
- Optional: `--features channel-matrix` for Matrix E2EE (adds matrix-sdk)

### Cross-Compilation Prerequisites (macOS Host)

```bash
brew install filosottile/musl-cross/musl-cross   # Linux musl targets
cargo install cargo-xwin                          # Windows MSVC targets
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
                  x86_64-pc-windows-msvc aarch64-pc-windows-msvc \
                  x86_64-apple-darwin
```

---

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
