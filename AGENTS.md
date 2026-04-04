# AGENTS.md -- AI Collaboration Guide for RsClaw

This file provides context for AI coding assistants (Claude Code, Cursor, Copilot, etc.) working on the RsClaw codebase.

## Project Overview

RsClaw is a high-performance AI gateway for distributed A2A orchestration and OpenClaw ecosystems written in Rust, with a Tauri-based desktop app. It is a ground-up rewrite of OpenClaw (TypeScript/Node.js) with full protocol compatibility.

## Architecture

```
src/
  agent/       # Agent runtime, memory, tool dispatch, loop detection, preparse commands
  channel/     # 13 messaging channels: Telegram, WeChat, Feishu, DingTalk, QQ, Discord, Slack, WhatsApp, Signal, LINE, Zalo, Matrix, WeCom
  config/      # JSON5 loader, schema definitions, runtime config resolution
  gateway/     # Startup orchestration, hot-reload, channel wiring
  server/      # Axum HTTP server, REST API, OpenAI-compatible endpoints
  provider/    # LLM providers: OpenAI, Anthropic, Gemini, DeepSeek, Qwen, Ollama, failover
  store/       # redb KV + tantivy full-text + hnsw_rs vector search
  ws/          # WebSocket protocol v3 (OpenClaw compatible)
  cmd/         # CLI commands: setup, configure, channels, cron, etc.
  acp/         # ACP protocol (Agent Client Protocol)
  a2a/         # Google A2A v0.3 protocol (cross-network agent collaboration)
  mcp/         # MCP client (Model Context Protocol, stdin/stdout JSON-RPC)
  plugin/      # Plugin shell bridge, hook registration
  skill/       # Skill loader, ClawHub/SkillHub client
  cron/        # Cron job scheduler and delivery
  browser/     # CDP headless Chrome automation
  i18n.rs      # Internationalization (10 languages: cn, en, ja, ko, th, vi, fr, de, es, ru)
  events.rs    # Event bus for agent/channel communication

ui/
  app/         # Next.js frontend (chat UI + control panel)
  src-tauri/   # Tauri v1 desktop app shell + Rust commands
```

## Tech Stack

- **Language**: Rust (Edition 2024, MSRV 1.91)
- **Async runtime**: Tokio
- **HTTP server**: Axum
- **Desktop app**: Tauri v1
- **Frontend**: Next.js + React
- **Storage**: redb (KV), tantivy (full-text search), hnsw_rs (vector)
- **Config format**: JSON5

## Coding Rules

### Rust

- Use `async fn` directly in traits. Never use the `async-trait` macro (Rust 2024 supports native async fn in traits).
- No emojis in code, comments, logs, or commit messages.
- All string-based i18n goes through `src/i18n.rs`. Supported languages: cn, en, ja, ko, th, vi, fr, de, es, ru.
- Config fields use `camelCase` in JSON5 and `snake_case` in Rust structs via `#[serde(rename_all = "camelCase")]`.
- Secrets in config support `SecretOrString` -- either a plain string or `{ source: "env", id: "VAR_NAME" }`.
- Channel message handlers follow the pattern: group policy check -> DM policy check (pairing/allowlist) -> per-user queue -> agent dispatch.
- Provider implementations go in `src/provider/` and must implement the OpenAI-compatible chat completions interface.

### Frontend (UI)

- Tauri v1 API: use `window.__TAURI__?.invoke` (NOT `window.__TAURI__?.core?.invoke` which is v2).
- React hooks must all be declared before any early `return` statement.
- The control panel is in `ui/app/components/rsclaw-panel.tsx` -- contains all panel pages.
- Config read/write in desktop mode goes through Tauri commands (`read_config_file`, `write_config`), not gateway API.
- Auth token priority: `gateway.auth.token` config > `RSCLAW_AUTH_TOKEN` env var > localStorage cache.

### Build

- Set `RSCLAW_BUILD_VERSION` and `RSCLAW_BUILD_DATE` env vars before building.
- Windows targets use static CRT linking (configured in `.cargo/config.toml`).
- Tag before build, not after. If filenames are wrong, rename -- don't rebuild.
- Cross-compile targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`.

### Testing

- Integration tests are in `tests/`.
- Run with: `RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo test`
- Use `--dev` flag (port 18889) when running the gateway during development.

## Key Patterns

### Adding a new channel

1. Create `src/channel/{name}.rs` implementing the `Channel` trait
2. Add config struct in `src/config/schema.rs` with `#[serde(flatten)] pub base: ChannelBase`
3. Add startup function `start_{name}_if_configured()` in `src/gateway/startup.rs`
4. Wire DM policy enforcer (pairing/allowlist/open/disabled)
5. Add to channel list in UI (`rsclaw-panel.tsx`)

### Adding a new tool

1. Add `ToolDef` in `build_tool_list()` in `src/agent/runtime.rs`
2. Add dispatch case in the tool match block (same file)
3. Implement `tool_{name}()` method on the agent runtime

### Adding a new LLM provider

1. Add provider config in `src/provider/registry.rs`
2. Provider must speak the OpenAI chat completions protocol
3. Add to `ALL_PROVIDERS` in `ui/app/components/onboarding.tsx` for UI support

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/v1/message` | POST | Send message to agent (full runtime) |
| `/api/v1/agents` | GET/POST | List/create agents |
| `/api/v1/agents/{id}` | PATCH/DELETE | Update/delete agent |
| `/api/v1/sessions` | GET | List sessions |
| `/api/v1/channels/pair` | POST | Approve pairing code |
| `/api/v1/channels/pairings` | GET | List pending/approved pairings |
| `/api/v1/config` | GET/PUT | Read/write config |
| `/api/v1/health` | GET | Health check |
| `/v1/chat/completions` | POST | OpenAI-compatible endpoint |
| `/v1/models` | GET | OpenAI-compatible model list |
| `/.well-known/agent.json` | GET | A2A Agent Card discovery |
| `/api/v1/a2a` | POST | A2A JSON-RPC task endpoint |

## License

AGPL-3.0. All contributions must be compatible with this license.
