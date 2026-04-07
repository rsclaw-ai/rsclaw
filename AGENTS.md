# AGENTS.md — RsClaw AI Collaboration Guide

AI coding assistant reference for Claude Code, Cursor, Copilot, and sub-agents.
Read this file before touching any code.

---

## Project Snapshot

RsClaw is a Rust-native AI agent gateway — a ground-up rewrite of OpenClaw (TypeScript) with full protocol compatibility, distributed A2A orchestration, and a Tauri desktop shell.

```
src/
  agent/     # Agent runtime, memory, tool dispatch, loop detection
  channel/   # 13 channels: Telegram WeChat Feishu DingTalk QQ Discord Slack
             #              WhatsApp Signal LINE Zalo Matrix WeCom
  config/    # JSON5 loader, schema, runtime resolution
  gateway/   # Startup orchestration, hot-reload, channel wiring
  server/    # Axum HTTP + REST + OpenAI-compatible endpoints
  provider/  # LLM providers: OpenAI Anthropic Gemini DeepSeek Qwen Ollama + failover
  store/     # redb KV + tantivy full-text + hnsw_rs vector
  ws/        # WebSocket protocol v3 (OpenClaw-compatible)
  acp/       # Agent Client Protocol
  a2a/       # Google A2A v0.3 (cross-network agent collaboration)
  mcp/       # MCP client (stdin/stdout JSON-RPC)
  plugin/    # Shell bridge, hook registration
  skill/     # Skill loader, ClawHub/SkillHub client
  cron/      # Cron scheduler and delivery
  browser/   # CDP headless Chrome automation
  cmd/       # CLI commands
  events.rs  # Global event bus
  i18n.rs    # 10 languages: cn en ja ko th vi fr de es ru

ui/
  app/       # Next.js chat UI + control panel
  src-tauri/ # Tauri v1 desktop shell + Rust commands
```

**Stack:** Rust 2024 · MSRV 1.91 · Tokio · Axum · Tauri v1 · Next.js · redb · tantivy · JSON5

---

## Team Roles

This project uses role-based AI sub-agents. Each role has a strict scope.
Role files live in `.claude/roles/`. Activate with: `cp .claude/roles/<role>.md CLAUDE.md`

| Role | Scope | Can Write |
|------|-------|-----------|
| `architect` | Requirements → interface definitions | `docs/` only |
| `backend-dev` | Rust implementation | `src/` |
| `ui-dev` | Frontend implementation | `ui/` |
| `backend-tester` | Rust test coverage | `tests/` |
| `ui-tester` | Frontend test coverage | `ui/test/` |
| `reviewer` | Rust code review | `docs/reviews/` |
| `design-reviewer` | UI/UX review | `docs/reviews/` |
| `qa-lead` | Quality gate, merge approval | PR description |

**Cross-role rule:** Never write outside your assigned scope. If a gap requires touching another scope, file a handoff note in `docs/interfaces/` and stop.

---

## Coding Standards

### Rust

```
- async fn in traits: native (Rust 2024). Never use async-trait macro.
- Error handling: never unwrap(). Use ? or .expect("reason") with explanation.
- No emojis anywhere: code, comments, logs, commit messages.
- i18n: all user-facing strings through src/i18n.rs only.
- Config fields: camelCase in JSON5, snake_case in Rust via #[serde(rename_all = "camelCase")]
- Secrets: SecretOrString — plain string or { source: "env", id: "VAR_NAME" }
- Channel handler order: group policy → DM policy (pairing/allowlist) → per-user queue → agent dispatch
- New events: must be registered in events.rs before use
- New pub API: must have doc comment
```

### Frontend (ui/)

```
- Tauri invoke: window.__TAURI__?.invoke (v1 API, NOT core?.invoke)
- Hooks: all declared before any early return
- Data fetching: never fetch() inside components — use hooks or store
- WebSocket: all WS logic through ui/src/hooks/useRsClawSocket.ts
- Config (desktop): Tauri commands read_config_file / write_config, not gateway API
- Auth token priority: gateway.auth.token config > RSCLAW_AUTH_TOKEN env > localStorage
- Components: Container (data) + Presenter (render) separation for complex views
- Styling: Tailwind utility classes only, no inline style, no hardcoded color values
- State: every async component handles loading / error / empty
```

### WebSocket (ws/)

```
- event:chat must be broadcast to ALL operator connections, not just initiating connection
- Connection states: connecting → connected → disconnected → reconnecting → error
- UI must reflect all 5 states with distinct feedback
- Operator connections are separate from user connections — different hook instances
```

### Build

```
- Env vars required: RSCLAW_BUILD_VERSION, RSCLAW_BUILD_DATE
- Tag before build, not after
- Dev gateway flag: --dev (port 18889)
- Test command: RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo test
- Cross-compile targets:
    aarch64-apple-darwin       x86_64-apple-darwin
    x86_64-unknown-linux-musl  aarch64-unknown-linux-musl
    x86_64-pc-windows-msvc     aarch64-pc-windows-msvc
```

---

## Key Patterns

### New Channel

```
1. src/channel/{name}.rs         — implement Channel trait
2. src/config/schema.rs          — add config struct with #[serde(flatten)] pub base: ChannelBase
3. src/gateway/startup.rs        — add start_{name}_if_configured()
4. Wire DM policy enforcer        — pairing / allowlist / open / disabled
5. ui/app/components/rsclaw-panel.tsx — add to channel list
6. tests/channel_{name}.rs       — create test file (even if skeleton)
```

### New Tool

```
1. src/agent/runtime.rs          — add ToolDef in build_tool_list()
2. src/agent/runtime.rs          — add dispatch case in tool match block
3. src/agent/runtime.rs          — implement tool_{name}() method
```

### New LLM Provider

```
1. src/provider/registry.rs      — add provider config
2. Implement OpenAI chat completions protocol
3. ui/app/components/onboarding.tsx — add to ALL_PROVIDERS for UI
```

---

## Review Standards

Reviewers output to `docs/reviews/[branch].md` using these tags:

| Tag | Meaning |
|-----|---------|
| `[BLOCK]` | Must fix before merge |
| `[SUGGEST]` | Recommended improvement |
| `[NOTE]` | Non-blocking observation |

**Auto-BLOCK triggers (Rust):** unwrap() without explanation · silent error discard (`let _ =`) · new WS event not in events.rs · pub fn missing doc comment · channel change with no corresponding test file

**Auto-BLOCK triggers (UI):** hardcoded color values · operation with no loading feedback · WS disconnect not disabling input · breaking change without confirm dialog

---

## QA Gate (qa-lead only)

Merge only when ALL of the following pass:

```
Backend
  □ docs/reviews/[branch].md — zero [BLOCK] items
  □ cargo test --all passes
  □ New features have tests/ coverage
  □ events.rs has no orphaned events

Frontend
  □ docs/reviews/ui-[branch].md — zero [BLOCK] items
  □ yarn test passes
  □ yarn tsc --noEmit passes
  □ WS state machine: all 5 states tested

Docs
  □ API changes reflected in docs/interfaces/
  □ Architecture decisions recorded in docs/adr/
  □ README / AGENTS.md updated if needed
```

If any QA check is ambiguous or a breaking change touches `ws/` or `provider/`, **stop and wait for human decision**.

---

## API Reference

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/v1/message` | POST | Send message to agent |
| `/api/v1/agents` | GET/POST | List / create agents |
| `/api/v1/agents/{id}` | PATCH/DELETE | Update / delete agent |
| `/api/v1/sessions` | GET | List sessions |
| `/api/v1/channels/pair` | POST | Approve pairing code |
| `/api/v1/channels/pairings` | GET | List pairings |
| `/api/v1/config` | GET/PUT | Read / write config |
| `/api/v1/health` | GET | Health check |
| `/v1/chat/completions` | POST | OpenAI-compatible |
| `/v1/models` | GET | OpenAI model list |
| `/.well-known/agent.json` | GET | A2A Agent Card |
| `/api/v1/a2a` | POST | A2A JSON-RPC tasks |

---

## License

AGPL-3.0. All contributions must be license-compatible.
