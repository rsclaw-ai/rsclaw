# Changelog

All notable changes to RsClaw will be documented in this file.

## Unreleased

### Knowledge Base MVP (Weeks 1–5 + polish) — spec §J/§K/§L/§S/§3/§5 complete

Five-week build of the user-managed RAG knowledge base, plus polish.
ADR `docs/adr/0001-knowledge-base.md`, spec
`docs/specs/2026-05-19-knowledge-base.md`, week plans under
`docs/plans/2026-05-19-kb-mvp-week{1..4}-*.md`. Module root
`src/kb/`; CLI at `src/cli/kb.rs` + `src/cmd/kb.rs`.

**Storage & ingest (spec §J atomicity):**

- 13 redb tables (`kb_docs`, `kb_chunks`, `kb_chunk_by_logical`,
  `kb_ledger`, `kb_jobs_*`, `kb_seen_items`, `kb_sync_state`,
  `kb_entities`, `kb_entity_index`, …) with composable
  `&WriteTransaction` accessors per table. `KbStore` facade owns the
  database handle.
- `ingest_canonicalized` writes `KbDoc + VersionPointer +
  IngestLedgerEntry + Job + SeenItems` in **one** redb tx. NOOP
  re-check + version compute + old_paths lookup happen inside the
  same wtx (race-safe — concurrent ingests of the same `(lsid,
  raw_sha)` produce exactly one doc).
- Content store at `md/<kind>/<slug>--<lsid8>--<md8>.md` is
  content-addressed; same lsid + new bytes lands at a different file
  so v2 ingest doesn't collide with v1.
- Tombstoned docs (spec §6's 30-day retention) resurrect on
  same-content re-ingest.
- `WorkerPool` (tokio single task + `block_in_place`) drains
  ChunkAndEmbed jobs. Fencing-token `mark_done` / `mark_failed`
  refuses transitions from a stale claim; `reclaim_stale` resets
  expired claims to Ready; bounded `shutdown()` exits in
  `poll_idle + scheduling slack`.

**Retrieval (spec §3 + §K + §L):**

- Hybrid: HNSW (`hnsw_rs` 0.3) for dense + tantivy 0.22 BM25 for
  sparse, fused by RRF (k=60), diversified by MMR (λ=0.5 default).
  `KbIndex` composes both layers; `rebuild::from_redb` is the
  canonical recovery path (redb is source of truth).
- CJK tokenizer (`JiebaTokenizer`, jieba-rs) registered as
  tantivy's `cjk` analyzer so Chinese queries actually match.
- Visibility filter (`search::filter::keep_doc`) runs on every
  retrieval path. `CallerScope` is a runtime-injected function
  argument; agent input cannot supply it. Stale-version filter
  drops chunks from superseded `doc_version`s.
- HNSW snapshot persistence — `kb compact` dumps to
  `<paths.root>/hnsw/snapshot.*` (`hnsw_rs::file_dump` + a JSON
  sidecar with `id_to_chunk`); `KbIndex::open_and_rebuild` tries
  `restore()` before `rebuild()`.
- Five MCP-shaped tools: `kb_search`, `kb_fetch`, `kb_list_docs`,
  `kb_similar`, `kb_search_entities`. JSON-friendly IO, no
  caller-controlled scope override.
- Regex entity extraction (URLs / emails / hashtags / @-mentions,
  CJK-aware) runs in the chunk_embed handler and populates
  `KbEntityIndex` edges. `require_entities` / `boost_entities`
  filters in the search pipeline use those edges. `kb_search`
  emits `entity_alignment` + warnings keyed off a query-side regex
  pass so the agent spots cross-entity hallucinations.

**Syncers + compactor (spec §S + §J Compactor):**

- `KbSourceSyncer` trait + `SyncContext`. `ManualUploadSyncer`
  (file → ingest), `UrlSyncer` (reqwest with conditional GET via
  ETag/Last-Modified, content-hash fallback). `SyncRegistry` for
  load/save of `SyncState`.
- Compactor (`run_compactor_tick`) does orphan-file scan with
  grace window + ledger state advance (`IndexingComplete →
  CleanupPending → Done`). Safe to run anytime.

**CLI (spec §5 v1 complete):**

`rsclaw kb add | ls | rm | search | show | visibility | compact |
stats | export | sync-all`.

- `kb add` supports files, directories (`--recursive --ext`), and
  URLs (auto-detected by scheme). Synchronously drains the worker
  pool in CLI-only mode so a follow-up `kb search` sees fresh
  chunks.
- `kb rm` accepts either a `doc_id` or `--tag <name>` for bulk
  tombstone (with `--yes`).
- `kb show` resolves both `doc_id` (lists chunks + metadata) and
  `chunk_id` (fetches with neighbor expansion).
- `kb search --json` emits the full `KbSearchOutput` for piping.
- `kb sync-all [--interval-min N] [--max M] [--dry-run]` refreshes
  every Active URL doc whose `SyncState.last_sync_at` is stale.

**Tests:** 215 unit + 25 integration across 7 test files
(`kb_week1_e2e`, `kb_week2_pipeline`, `kb_week2_recovery`,
`kb_week3_search`, `kb_week4_syncers`, `kb_week4_compactor`,
`kb_entities_e2e`, `kb_cli_smoke`). 0 ignored, 0 failed.

**Deferred to Week 6 / V2 (not in MVP):**

- BGE-M3 real embedder (today: `StubEmbedder` — deterministic
  sha256-derived 1024-dim vectors; same `KbEmbedder` trait
  interface).
- Gateway-resident syncer scheduler (today: `kb sync-all`
  invoked manually or from user cron).
- ML-based NER (today: regex extractor for URLs / emails /
  hashtags / @-mentions).
- LocalFolder / Mail / Chat syncers, Tauri admin UI, `kb_explain`
  trace tool, citation_confidence, recency_policy, Reranker — all
  spec V2.

### rsclaw protocol §2.4 — in-place compact splice

- New trait method `LlmProvider::compact_splice` (default `Err` for
  non-rsclaw providers). `RsclawProvider` implements it as
  `POST /v1/agent/sessions/<id>/compact` with body
  `{keep_head_messages, summary, keep_tail_messages, expected_msgs_count}`.
  Server keeps head/tail KV pages, drops the middle, prefills the new
  summary, returns the same `session_id` (slot survives compaction).
- `compact_inner` integrates splice: on success the cached
  `SessionEntry.last_seen_msgs_len` is updated to the post-splice value
  so the next turn does NOT misinterpret the local `msgs.len()` drop as
  a history-trim signal and force a replay. On `Err` (server doesn't
  support the §2.4 shape yet, 409 drift, 410 evicted, etc.) gateway
  falls through to the legacy replay path via `lookup_and_bump`.
- Each summary now embeds `[CONTEXT COMPACTION compacted at
  YYYY-MM-DDTHH:MM]` so the model has a recent-verbatim-vs-summarized
  temporal anchor; the head's `[Session started: ...]` marker remains
  byte-stable across iterative compactions (head sanctuary).

### `prefix_id` is config-driven (no longer derived from `req.model`)

- New `RSCLAW_DEFAULT_PREFIX_ID = "rsclaw/2026.5.5"` constant.
- `RsclawProvider::with_prefix_id` builder + `ProviderConfig.prefix_id`
  config field thread the override through `gateway/providers.rs`.
- Builder validates §2.10.1 (exactly one `/` separator) and silently
  falls back to the default on malformed input; typos in config no
  longer survive gateway boot.
- `split_request` takes `prefix_id: &str` verbatim — `req.model` no
  longer participates in prefix-cache identity, restoring the per-version
  static prefix-cache hit rate that the old derivation accidentally
  fragmented per-model.

### Shared system prefix restructuring (byte-stability for static cache)

- `## CAPABILITY PRIORITY` (plugin/skill > built-in tools priority +
  failure-mode warning + screenshot routing) moved from
  `cached_skills_system` to `build_shared_system_prefix`. Ships
  unconditionally even on 0-plugin / 0-skill installs so the shared
  prefix bytes are stable across every client of the same version —
  required for rsclaw-server's static-prefix-cache lookup to hit.
- Dropped the duplicated `"Priority: plugins > skills > built-in tools"`
  line from `build_plugins_system` (now covered by shared CAPABILITY
  PRIORITY).

### `/reset` removed

- **Breaking user-visible change**: `/reset` is no longer recognized as
  a slash command. The previous semantic (wipe session + save summary
  to memory, but no archive / no generation bump) was a weaker `/new`
  variant with two inconsistent code paths (`reset_signal` consumed in
  runtime vs `__RESET__` pseudo-token in chat-stream) that drifted on
  whether the memory save actually happened.
- Users wanting the "wipe + save summary" behavior should now use
  `/new` (additionally archives the old session into a new generation
  for later retrieval, and invalidates the plugins/skills cache).
- Code removed: `reset_signal` AtomicBool on `AgentHandle`, its consumer
  block, the `__RESET__` chat-stream handler, the `session_reset` i18n
  key, all help-text entries (CLI + `docs/lang/README_*.md`), and the
  `/reset` allowlist entry.
- The "compaction insufficient — suggest reset" hint string now points
  users at `/new` instead of `/reset`.

### `/watch` — live event stream → chat slash command

- `/watch <source>` subscribes to a file tail, SSE stream, or shell
  subprocess and streams events directly back to chat — does **not**
  invoke the agent, so zero LLM cost and near-realtime delivery.
- Source kinds auto-detect from the first token: URLs route to SSE,
  paths route to file, raw commands require explicit `shell` prefix.
  Cross-platform `file` source uses 200 ms polling + inode/size
  rotation detection (works on Linux/macOS/Windows without `tail`).
- Composition with `/loop`: `/loop 10m /watch /var/log/x.log`
  re-spawns the watch after gateway restarts via `/loop`'s cron
  replay; dedup keyed on `(channel, peer, normalize(source))` so
  repeat invocations are no-ops.
- SSE client compatible with `quick_stream.py`: standard wire-format
  parser, `${VAR}` substitution in URL + headers (empty values rejected),
  2s→30s exponential backoff with no retry cap, `Last-Event-ID` client
  support, `4xx` fatal, 90s no-byte heartbeat watchdog, `Accept-Encoding:
  identity` to forbid gzip buffering.
- Rate limit defaults to 1 event / 2s + batching (`N more events in 2s`);
  override with `--rate 0` for unfiltered streams. `--grep <regex>`
  for line-level filtering. Per-(channel, peer) cap of 5 concurrent
  watches.
- Adds `PreparseOrigin::{User, Cron}` to `try_preparse_locally` so
  cron-replayed `/watch` dedup-hits are delivered silently (no chat
  spam). Empty `OutboundMessage` from preparse is the silent signal;
  the existing empty-text short-circuits in all 14 channel callsites
  already suppress delivery.
- `/watch list`, `/watch stop <id>`, `/watch stop all` for management.
  In-memory only — restart clears registry; cross-restart durability
  comes from `/loop` composition, not from `/watch` itself.

## [2026.5.1] - 2026-05-01

### Voice end-to-end via sherpa-onnx

- Sherpa-onnx STT (`paraformer-zh` int8 / full, `whisper-turbo`,
  `whisper-tiny`) wired into `transcribe_audio` as the preferred provider.
  Detected by inspecting `<base>/tools/sherpa-onnx/bin/sherpa-onnx-offline`
  + a model directory with the expected file layout. JSON output parser
  handles sherpa 1.13+ (`{"text": "..."}`) and legacy `text:` formats.
- Sherpa-onnx TTS (`vits-melo-tts-zh_en` default, `vits-zh-aishell3`,
  `vits-theresa` lightweight fallback) auto-discovered via explicit
  priority list. VITS lookup picks up `lexicon.txt`, optional `dict/`,
  and joins all `.fst` rule files comma-separated into `--tts-rule-fsts`.
- All sherpa CLI invocations now use `--flag=value` argv format
  (sherpa-onnx 1.13's parse-options enforces single-token options and
  rejects `--flag value` splits).
- Voice-tag plumbing: feishu and wechat channels prefix transcribed
  voice text with `[__VOICE_INPUT__]`. Runtime detects the prefix at
  `run_turn` entry, enables `voice_mode_sessions`, and strips the tag
  before storing the message — auto-TTS replies in audio without
  requiring channels to thread audio bytes through.
- Auto-TTS suppresses LLM `send_file` calls for audio paths in
  voice mode (was producing duplicate audio messages from stale TTS
  files).
- One-shot install hints: when STT or TTS fall back to non-sherpa
  paths and `<base>/var/install-hints.json` doesn't already mark the
  feature, append a localized hint with the install command. zh / en
  / ja localized via `i18n.rs`.

### WeChat audio delivery fallback chain

- Try silk + `send_voice_message` first (200 OK at API but client
  doesn't render).
- Fall back to ffmpeg audio→mp4 (black 320×240 + AAC + faststart)
  sent as `UploadMediaType::Video` (CDN periodically 5xx's, retry
  loop already handles).
- Fall back to `.wav` File attachment (always works, displays as
  file).
- New helper `audio_to_video_mp4` drives ffmpeg subprocess; failures
  surface as `warn!` and continue down the chain.

### Site-rule injection

- `web_fetch` short-circuits the HTTP request and returns the
  matching site-rule body on first hit per host (5-min LRU). Agent
  receives a `next_action` field instructing it to make the
  rule-prescribed call.
- `web_browser action=open` inlines the rule body into the response
  so the agent has the playbook before snapshotting.
- `applicable_site_rules` now tries the second-to-last host label so
  `api.stackexchange.com` resolves to `stackoverflow/` (the previous
  leftmost-label-only logic missed every subdomain).

### Bootstrap prompts

- `SOUL.md` / `AGENTS.md` / `IDENTITY.md` (zh + en) rewritten with
  explicit anti-hallucination self-check sections.
- New voice-reply rules: don't say "click the attachment" when the
  auto-TTS bubble plays inline; don't call `send_file` for audio
  under voice_mode.

### Sherpa-onnx bundling

- `cmd/tools.rs` now requests the `*-shared.tar.bz2` variant per
  platform (with CLI binaries) instead of `*-shared-lib` (lib-only).
  linux-aarch64 uses `-shared-cpu`; win-x64 uses
  `-shared-MT-Release`.
- `cmd/models.rs` registers `vits-melo-tts-zh_en` under `vits` /
  `tts` / `melo` aliases (default), keeps theresa as `theresa`.
  Adds `paraformer-zh` int8 + full precision entries
  (2025-10-07 release).

### Doubao defaults

- Default API protocol is now `openai-responses` (Ark Seed family
  native protocol with tool calling) instead of OpenAI Chat. Custom
  / codingplan still default to OpenAI Chat.
- Default model: `doubao/doubao-seed-2.0-pro`. Onboarding wizard
  also surfaces `doubao-seed-2.0-lite`.
- Config save (panel + onboarding) now persists `api: openai-responses`
  even when the user never opens the API Type dropdown.

### `/cron remove`

- Handled in gateway preparse with `remove`/`rm`/`delete`/`del`
  aliases; matches by 1-based index or job id. Returns localized
  confirmation text. Was previously falling through to the LLM and
  getting silently swallowed.

### UI

- Chat virtualisation via react-virtuoso (long sessions no longer
  freeze).
- Stuck-stream watchdog with 5-min activity threshold and
  force-send escape hatch.
- Settings → Danger zone reworked: drop dead "Reset all settings",
  add "Clear chat history" (idb only) and "Clear local cache"
  (sessionStorage + WebKit cache, preserves language).
- NewChatDialog remembers last-picked agent in localStorage.
- Tray menu i18n (zh / en) reading `gateway.language` from
  `rsclaw.json5`, falling back to `LANG` env, then English.
- Config panel auto-marks dirty when `gateway.language` is missing
  so first save persists the dropdown default.

### Computer use

- HiDPI mouse coordinate correction on macOS via cached
  osascript+screencapture probe.
- Screenshot region (`--region x,y,w,h`) and resize (`--size W H`)
  flags.

### Browser

- `cmd_wait` with no target/value degrades to a pure sleep instead
  of timing out on an empty CSS-selector predicate.

### CI

- `clippy -D warnings` clean across lib + tests.
- Removed `unwrap()` in `tools_computer` screenshot fallback.
- `non-interactive setup` logs `seed_tools` failures instead of
  swallowing them.

### Browser-harness skills

- 80 browser-harness MIT skills (Amazon, Reddit, TikTok,
  TradingView, Xiaohongshu, etc.) imported under
  `tools/web_browser/site-rules/`. Embedded at compile time via
  `include_dir!`; extracted on first run, preserves user
  hand-edits.

## [2026.4.29] - 2026-04-29

### Shell-bridge plugins are now first-class LLM-callable

Node / Bun / Deno plugins are exposed to the LLM as `<plugin>.<tool>` — the
same namespace as wasm plugins (wasm wins on collision). Previously shell
plugins existed only for hooks and slot fills.

- Bidirectional shell-bridge JSON-RPC protocol: plugins can call host methods
  (`notify`, `log`, `browser_*`, `sleep`, `storage_allocate_artifact`) by
  writing JSON-RPC requests with **negative ids** to stdout. Existing one-way
  positive-id usage (hooks, slot fills) keeps working unchanged.
- New module `src/plugin/host_methods.rs` containing the `HostMethodRegistry`,
  the dispatcher for plugin-initiated requests.
- Reader task in `shell_bridge.rs` owns stdout and demuxes lines by id sign;
  pending-request map correlates host-initiated calls with their responses.
- A2 host method catalog (full parity with wasm host functions): `notify`,
  `log`, `browser_open` / `browser_eval` / `browser_eval_with_args` /
  `browser_click` / `browser_click_at` / `browser_fill` / `browser_snapshot` /
  `browser_download`, `sleep`, `storage_allocate_artifact`. Browser session is
  shared between wasm and shell plugins so login state persists across runtimes.
- Documentation: [`docs/plugin-development.md`](docs/plugin-development.md)
  covers the wire protocol, host method catalog, and authoring tradeoffs.

### Changed

- `shell_bridge::Plugin::spawn` now requires a second `Arc<HostMethodRegistry>`
  argument.
- `load_all_plugins` now requires a fourth `notify_tx` argument; gateway
  startup wires `notification_tx` through.
- `tools_builder::build_plugins_system` now also takes shell plugins and
  sorts blocks by name for byte-stable output.
- `PluginRegistry`: `get` renamed to `get_shell` (no external callers existed
  under the old name); added `shell_plugins_iter`.
- `shell_bridge::ShellBridgePlugin::shutdown` now awaits the reader task
  after killing the subprocess so in-flight responses drain cleanly.

### Backward compatibility

- Existing wasm plugins are unaffected.
- Existing shell plugins used only for hooks/slots continue to work — the
  bidirectional layer is a strict superset of the old one-way protocol.

## [2026.4.28] - 2026-04-28

### Skills

- **iWenCai SkillHub** — added as a native skill registry alongside
  clawhub.ai / skillhub / skills.sh. Surfaces only the 22 `hithink-*`
  finance skills (market query, A股/港股/美股/ETF/期货/基金 selectors,
  finance/macro/event/industry queries, etc.). Hides the 67 internal
  devops skills. Install lands in `~/.rsclaw/skills/<slug>/`.
- **`use_skill` function-call tool** — first-class entry the LLM can
  pick alongside `web_fetch` / `execute_command`. Returns the full
  SKILL.md untruncated (60KB cap) so the model sees the actual CLI
  contract instead of inventing flag names.
- **Lazy SKILL.md loading** — system prompt injects only frontmatter
  description + dir path; full body loads on demand via `use_skill`.
  Cuts a 22-skill install from ~264KB injected to ~4KB.
- **`rsclaw skills update`** uses `install_with_fallback` so
  iwencai/skillhub-installed skills re-resolve correctly (was
  clawhub-only).
- **`skillRegistries` config schema** — per-registry `apiKey` /
  `baseUrl` overrides in `rsclaw.json5`; resolved values exported to
  process env so spawned skill subprocesses (Python CLIs etc.) inherit
  transparently.
- **Site-rules** moved from per-workspace to
  `~/.rsclaw/tools/web_browser/site-rules/`, shared across agents.

### Channels

- **Slack file upload V2** — `files.upload` v1 was disabled by Slack
  (`{"ok":false,"error":"method_deprecated"}`). Rewrote to the
  3-step `getUploadURLExternal` → multipart PUT →
  `completeUploadExternal` flow.
- **Discord/Slack inbound attachments** — images now reach the vision
  model and PendingFile flow (was just a text marker
  "image_attachment_received"). Files forwarded as `FileAttachment`
  for analyze/save prompt.
- **Discord image MIME** — handles webp/gif/bmp/svg via
  `parse_data_url`; http URLs route through `embeds[].image.url`;
  filename extension matches MIME for inline preview.
- **Discord file MIME** — uses tool-supplied MIME instead of
  hardcoded `application/octet-stream`, so video/audio/PDF render
  inline in Discord.
- **`\xxx → /xxx` alias** for Slack and Discord — clients eat a
  leading `/` for native slash-command UI.
- **Slack self-reply loop fix** — was processing its own messages as
  new user input, replying again, infinite loop. Filters by `bot_id`
  / `subtype == "bot_message"`.
- **Slack `/ss` image-only reply** — chat.postMessage with empty text
  returned `no_text` and `?` short-circuited the upload loop. Now
  skips post when text is empty.
- **Discord notification 404** — `chat_id` was always empty in
  `RunContext`, falling back to `peer_id` (user id) which Discord
  rejects with "Unknown Channel". `chat_id` now propagates from
  `AgentMessage` through `run_turn` to `RunContext`.
- **Shared attachment helpers** — `parse_data_url` / `mime_to_ext` /
  `pick_file_mime` extracted to `channel/attachments.rs`.
- **Policy rejection logs** — bumped from `debug` to `warn` across
  all 13 channel handlers so `groupPolicy: allowlist` (the default)
  silent drops are visible in the default log.

### Browser

- **`web_browser action=screenshot url=...`** — one-shot navigate
  + capture, single tool call.
- **`/webshot <url>`** preparse fast-path — headless-Chrome web-page
  screenshot. Auto-detects Chrome / Chromium / Edge / Brave on
  common install paths.
- **`/ss` desktop screenshot** routed clearly; system prompt tells
  agent NOT to call `web_browser screenshot` for plain "截图"
  requests.
- **Restart loop fix** — `restart()` now resets `last_activity` so
  the very next `execute()` doesn't immediately re-trigger
  idle-expiry on the pre-crash timestamp (was killing every
  freshly-launched Chrome and surfacing as "Chrome exited without
  printing DevTools URL" from jimeng / douyin plugins).

### Memory

- **Tier on insert** — high-importance docs (auto-capture phone
  numbers / IDs / IPs at importance=0.85) reach Core on insert
  instead of waiting for a search-touch. Crystallisation pipeline no
  longer starves.
- **Workflow crystallisation for hard turns** — meditation phase
  distils Core memory clusters into reusable SKILL.md files via
  `meditation_deps`.

### Cron

- **cron.json5 protected from being wiped** on parse failure — load
  returns `(jobs, parse_ok)`, all save sites check the flag, reload
  is also blocked. Prevents user customisations getting overwritten
  with empty content after a syntax error.
- **Saved-report file content in summarize** — when a script's stdout
  contains `报告已保存: xxx.md` / `saved to: xxx` / etc., the file
  content is read and fed to the summarise agent.
- **Summarize prompt rewritten** with strict anti-fabrication rules
  (English; per the new no-i18n-for-LLM-prompts convention).
- **`summarize=false` returns saved file content** too (was
  stdout-only).
- **Path dedup** when multiple regex patterns match the same saved
  file.

### System prompt

- **Workspace anchor** — agent now sees its own workspace path in
  the prompt and stops globally searching `~/.rsclaw/` for "my
  files".
- **Skills priority directive** — explicit `plugins > skills > tools`
  ordering, with a worked failure example (flight search → flyai,
  not web_fetch ctrip).
- **Screenshot routing** rules — `/ss` for desktop, `/webshot` for
  web, `web_browser screenshot` only after `open` in same session.

### i18n

- Localised 8 channel-facing strings: `session_cleared`,
  `session_reset`, `session_new`, `compact_done`,
  `compact_done_no_summary`, `compact_nothing`, `screenshot_failed`,
  `webshot_failed` — covering en/zh/th/vi/ja/es/ko/ru/fr/de.
- **Documented convention**: LLM-facing prompts (system messages,
  tool descriptions, summarize/analyze prompts) stay English-only,
  no i18n. User-facing channel strings still go through i18n.
  See `CLAUDE.md` / `AGENTS.md`.

### OpenClaw migration

- **Imports MEMORY.md + memory/*.md** workspace files (was
  session-only).
- **AGENT.md → AGENTS.md auto-promotion** when only the singular form
  is present.
- **Branding rewrite** of identity files: `OpenClaw → RsClaw`,
  `🦞 → 🦀`.
- **Staged progress display** — clear `Step 1/3 ...` banners during
  migration so users don't think "BGE download finishing = migration
  done".
- **`allowFrom` credential files** are now actually copied (was just
  printing a hint).
- **SKILL.md frontmatter sanitisation** — strips backtick-wrapped
  YAML values that yaml-rust rejected.

### Runtime

- **`SIGINT/SIGTERM` graceful drain** — gateway now installs a global
  signal handler that funnels through `ShutdownCoordinator`. Cron's
  own `ctrl_c` handler removed (it was eating SIGINT and the
  gateway/channels/axum saw nothing).
- **Per-agent context window** — resolved from
  `agent.model.contextTokens` → `agents.defaults.contextTokens` →
  64000 fallback. Used by both `/status` display AND pre-flight
  emergency-compaction check.
- **`chat_id` propagation** — `run_turn` accepts `chat_id`,
  `RunContext.chat_id` is real (not always empty), so notifications
  on group sessions land on the channel, not the user id.
- **Skip intermediate-text notification** on ws/desktop channels
  (those see the streaming text already).
- **Build cleanup** — 4 latent warnings cleared.

### UI / Tauri

- **Config save** no longer eats single-account channel blocks
  (Discord etc.) on save — zombie cleanup only deletes explicit
  `accounts: {}`, not missing-`accounts` legacy schema.
- **Async-task reply badge** in desktop chat (`[任务完成]` /
  `[Task done]` etc.) so the user can tell async results from
  in-band replies.

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
