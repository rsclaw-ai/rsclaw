# Changelog

All notable changes to RsClaw will be documented in this file.

## [2026.4.26] - 2026-04-26

### Unified Task Queue (all 13 channels)

- All channels (except CLI) now route through a persistent task queue
- `tokio::sync::Notify` zero-latency wake-up, replaces 500ms polling
- Concurrent workers (`tokio::spawn` per task) — multiple users no longer block each other
- Removed 2-second debounce; consecutive messages auto-merge in queue
- File attachments staged to disk so workers can recover full payload after restart
- `reply_to` quoting + `pending_analysis` background analysis preserved across queue
- Task command: `/task` slash command + natural-language detection
  ("帮我写一个..." auto-promotes to background task)

### Auto-Continue Supervisor Loop

- 24/7 agent operation: detects stuck/partial completions and auto-resumes
- Max 10 turns to bound runaway loops
- Distinguishes real completion from "I'll do that next" placeholder replies

### Graceful Restart

- ShutdownCoordinator drains in-flight work before swapping the listener
- WebSocket pushes restart event to UI; "Restart Required" banner under StatusPage
- 60-second auto-restart countdown with Restart Now / Later / Dismiss
- Fixed timing bug where replacement spawned before axum dropped its listener

### Hot-Reload (no restart banner)

- `agents.defaults` and `tools.*` field edits apply live
- Per-agent and global temperature config, live-applied
- Restart banner suppressed when only live-safe fields changed

### Cron / Scheduling

- Modifying a job now cancels the in-flight run with the old cadence
  (previously only delete/disable cancelled — editing 5min → 30min would double-fire)
- New `every_seconds` / `every_ms` tool args for sub-minute scheduling
- Graceful drain via ShutdownCoordinator; cron jobs not dropped on restart
- `/cron list` output cleaned up for IM-channel rendering
- `CANCEL_BY_RELOAD` sentinel keeps reload-cancelled runs out of error counts

### LLM Request

- `max_tokens` default raised to 30000 (was unset, causing some models to fall back to 8192)
- `json_f32()` fixes float precision (0.6 no longer serializes as 0.6000000238418579)

### WASM Plugin System (rewrite)

- Sandbox limits enforced (CPU / memory / wall-clock)
- `plugin.json5` manifest is now the single source of truth (no scattered config)
- Host API redesigned for clarity and forward compatibility
- New `host::notify` routes through the agent's `notification_tx`
- WASM plugin tools wired into agent dispatch + browser primitives

### Channels

- Silent QR-login variants for headless callers (no terminal QR rendering)
- Channel config screen: Feishu QR login now works (previously was a "use terminal" toast)
- Channel config screen: fixed WeChat HTTP field-name mismatch
  (`qrcode_img_content` → `qrcode_url`, status `confirmed` → `ok`)

### Browser

- `cmd_download` URL mode (server-side fetch with session cookies)
- Fixed Safari UA reporting; no longer shadows `result` with `url` field
- UI tools tab: run-mode select (auto / foreground / background)
- Transparent retry on CDP transport errors (already present, hardened)

### CLI

- Merged `agent` command into `acp` (single namespace for ACP control)
- `acp list` now shows active WS connections via HTTP API (no WS handshake)
- `acp list/kill` auto-detect auth token from config
- Friendly error when ACP endpoint unavailable (no naked panic)
- `message send/read/broadcast` implemented via gateway HTTP API; other ops return `unsupported`
- `message read` accepts full session key as target
- `sessions list` reads auth token from config (was failing without `--token`)
- `models download` supports 6 models, unified download from gitfast.org

### Agent / Memory

- Blocked LLM from voluntarily writing `kind=summary` memories
  (distillation path stays the sole writer)
- Review round 2 fixes: orphaned docs, silent error discard, token race
- Removed `processing_timeout` / `send_processing` (superseded by task queue)

### Desktop App + Infrastructure

- WS auth failure now refreshes token from config (fixes infinite-reconnect log spam)
- Tauri no longer auto-generates auth token in config
- `ConnHandle` stores client info for `acp list`
- Sidecar spawn (`channel_login_start`) wrapped in `hide_window` —
  no more flashing cmd console on Windows
- App icon shrunk via nested SVG viewport (78% content area, white border removed)
- Status-bar tray icon redesigned as solid filled silhouette (legible at 22×22)
- Window centers on launch
- `build.sh` ROOT_DIR resolves correctly from any directory
- Version unified to 2026.4.26
- `anycli` upgraded to 0.2
- Merged remote `dev`: WASM plugin + browser improvements

## [2026.4.20] - 2026-04-20

### License Change

- **Relicensed from AGPL-3.0 to MIT OR Apache-2.0** dual license for broader adoption

### Multi-Agent Architecture

- Four agent types with explicit `AgentKind`: Main, Named, Sub, Task
- **Bidirectional communication**: Main ↔ Named, Named ↔ Named (team mode)
- AgentKind-based permission control:
  - Main: full permissions (spawn/task/send/list/kill)
  - Named: full permissions except cannot kill Main
  - Sub: task + list only
  - Task: list only
- Main agent protected from kill
- Sub agents are memory-only (no workspace, no SOUL.md, no config persistence)
- Task/send timeout now uses configured `timeoutSeconds` (removed 5-min hard cap)
- Task agents killed on timeout to free resources
- Anti-loop protection: max 5 send depth between agents

### Browser Automation (agent-browser parity)

- **50+ browser actions** (was 20), full alignment with agent-browser
- New CLI: `rsclaw browser <command>` for direct browser control
- `capture-video` — capture video URLs using Content-Type detection (not regex)
- `download-video` — one-click video download with cookie extraction
- `annotate` — annotated screenshot with numbered element labels
- `inspect` — open Chrome DevTools
- Semantic locators: `getbytext`, `getbyrole`, `getbylabel`
- `console` — browser console messages
- `content` — full page HTML
- `frame` / `mainframe` — iframe navigation
- `waitforurl` — wait for URL change (login/redirect)
- `snapshot --compact --depth --selector` — snapshot filtering
- `requests` — list network requests
- `state-save` / `state-load` — auth persistence (cookies + localStorage)
- `auth save/login/list/show/delete` — credential vault
- `profiles` — list Chrome profiles
- `batch` — execute multiple commands in one session
- `session` — show/list debugging targets
- `tab new/list/close/switch` — tab management
- `get text/html/value/attr/count/box` — element property queries
- `keyboard type/inserttext` — keyboard input
- `download` — download by clicking element
- `connect <port|ws://>` — explicit CDP connection
- CLI defaults to headed mode on desktop
- Clean stdout/stderr separation (data to stdout, status to stderr)
- Removed WARN noise from CLI output

### AnyCLI Integration

- Built-in [anycli](https://crates.io/crates/anycli) v0.2 for structured web data extraction
- `rsclaw anycli run/list/info/search/install/update`
- 7 built-in adapters: hackernews, bilibili, github-trending, arxiv, wikipedia, douban, v2ex
- Agent auto-selects anycli when structured data adapter available
- Community hub support (search + install from GitHub)

### Web Search & Fetch

- Relevance-based deep fetch (only fetch results matching query terms)
- Browser fallback returns markdown (was plaintext)
- `clarify` tool for interactive user questions

### Cron / Scheduling

- One-shot timer support (`delay_ms` parameter for reminders)
- One-shot jobs auto-remove after execution
- Jobs now execute concurrently (was sequential blocking)
- Cancelled jobs terminate within 1 second (cancel flag polling)

### KV Cache / Performance

- `kv_cache_mode=2` incremental messages (cache_id + messages_append)
- cache_id generated by server (rsclaw-server), returned via X-Cache-Id header
- Accurate token counting includes system prompt + tools + skills overhead
- Pre-flight check before LLM request: emergency compact if approaching limit

### Skills

- Updated jimeng skill: `eval` → `evaluate`, state save/load
- Updated web-scan-login skill: state format aligned with web_browser API
- Updated ecommerce-search skill: state references updated
- Active skill matching per-turn (only matched skills injected into context)

### WASM Plugins

- Merged jimeng automation branch (WASM plugin system, wasmtime v29)
- Active skill matching with priority over default tool selection

### Infrastructure

- rsclaw-server design document (GPU inference scheduler for 10K+ nodes)
- rsclaw-llm: API key session binding + TTL for KV cache (verified on RTX 5090)
- Slot-level TTL eviction + API key isolation in cache reuse
- Design: P2P KV cache migration on node drain (no server relay)

## [2026.4.18] - 2026-04-18

### Features

- Cross-platform voice/video/file delivery (WeChat, Feishu, Telegram, WeCom, DingTalk, QQ)
- Browser CLI initial implementation
- Web browser interactive snapshot + new commands
- System prompt merging (single system message for model compatibility)
- Multi-engine web search (Baidu + Bing + Sogou, free, China-accessible)
- Ecommerce search skill (JD/Taobao/Tmall/Douyin)

### Fixes

- MiniMax compatibility (skip enable_thinking/thinking_budget params)
- Tool call pairing fix (remove orphaned tool_calls/results after compaction)
- Heartbeat restricted to memory tool only
- Cron job deletion takes effect immediately during execution
- CI clippy warnings resolved
