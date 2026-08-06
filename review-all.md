# RsClaw — Full Codebase Review

**Date:** 2026-07-16
**Scope:** All 33 workspace crates, root `src/`, `ui/`, `tests/`, `docs/`
**Method:** Multi-subagent exploration of every `.rs` file across the repository; read-focused (no edits).

---

## 1. Executive Summary

RsClaw is a **well-engineered, production-grade Rust codebase**. The architecture is clean: a workspace-split of 33 crates with clear responsibility boundaries. Error handling is overwhelmingly disciplined (`?` / `with_context`), string safety (CJK) is conscientiously handled, and critical paths (compaction, failover, A2A relay, KB ingest) carry thorough tests with documented incident-driven design rationale.

**Overall grade: B+** — Strong core, but several [BLOCK]-worthy issues exist in auth bypasses, silent data loss paths, and dead configuration.

### Severity Counts

| Severity | Count | Description |
|----------|-------|-------------|
| **[BLOCK]** | 23 | Must fix before production deployment |
| **[SUGGEST]** | 48 | Recommended improvement |
| **[NOTE]** | 60+ | Non-blocking observation |

---

## 2. [BLOCK] Issues — Must Fix

### 2.1 Authentication & Security

#### [BLOCK] WS Auth Bypass on Root Path `/`
- **Crate:** `rsclaw-runtime` — `server/mod.rs:587`, `handshake.rs:109-129`
- The `/` route is in the auth bypass list AND performs a WS upgrade. A client connecting to `/` with an `Upgrade: websocket` header bypasses all auth middleware that protects `/ws` and `/gateway-ws`, even when `gateway.auth.token` is configured.
- **Fix:** Either enforce the WS handshake auth regardless of entry path, or remove `/` from the bypass list for WS upgrades.

#### [BLOCK] Unauthenticated Device Token Minting
- **Crate:** `rsclaw-runtime` — `handshake.rs:291-299`, `handshake.rs:70-74`
- `issue_token` unconditionally mints a persistent device token for every connection, even when no auth token is configured (open gateway). These tokens are never revocable and grant permanent access.
- **Fix:** Add token expiry, revocation API, or require auth for device token minting when a gateway token is configured.

#### [BLOCK] WS Shutdown/Restart Unprotected
- **Crate:** `rsclaw-runtime` — `ws/methods/system.rs:612-634`
- `system.shutdown`/`system.restart`/`system.stop` via WS run unconditionally once the connection is established — no loopback check. HTTP equivalents (`/api/v1/shutdown`, `/api/v1/restart`) correctly enforce loopback-only. In open-gateway mode, any WS client can terminate the process.
- **Fix:** Add loopback or role check to WS shutdown methods.

### 2.2 Data Integrity & Silent Data Loss

#### [BLOCK] A2A TaskStore Auto-Reset on Open Failure
- **Crate:** `rsclaw-runtime` — `a2a/store.rs:28-74`
- On any open failure other than `DatabaseAlreadyOpen`, the store moves the user's database aside and silently recreates it — wiping all A2A task history, push configs, and owner records. This triggers on transient I/O errors and file corruption alike.
- **Fix:** Retry once, surface error to operator, or require explicit `--repair` flag rather than auto-recreating.

#### [BLOCK] A2A Completion Paths Silently Discard Store Errors
- **Crate:** `rsclaw-runtime` — `a2a/server.rs:380-382, 521-526`, `a2a/streaming.rs:370-409`
- `put_owner`, `append_artifact`, `set_status`, `delete_push_configs_for_task` — all completion-side writes silently discard `Result` with `let _ =`. A persistence failure leaves tasks permanently `Working` while the API reports success.
- **Fix:** At minimum, `warn!`/`error!` log the failures.

#### [BLOCK] Notification Router Exits Forever on `Lagged`
- **Crate:** `rsclaw-runtime` — `gateway/startup.rs:709`
- The notification router `while let Ok(msg) = rx.recv().await` treats `RecvError::Lagged(n)` as loop-exit, permanently killing ALL notification fan-out (desktop, WeChat, Feishu) for the process lifetime after one 65+ message burst.
- **Fix:** Handle `Lagged` with `continue` and a logged warning.

#### [BLOCK] Shutdown Notify Multi-Waiter Race
- **Crate:** `rsclaw-runtime` — `gateway/shutdown.rs:76-89`
- `tokio::sync::Notify` permits only one stored permit. `begin_drain` calls `notify_waiters()` which wakes all currently registered waiters but stores no permit. Multiple concurrent `notified()` callers race: late subscribers hang forever. Affects channel outbound loops, task queue worker, and axum graceful shutdown.
- **Fix:** Store a permit (`Notify::notify_one` + count) or use `watch`/`AtomicBool`.

#### [BLOCK] Silent Upload Data Loss
- **Crate:** `rsclaw-agent` — `runtime/run_turn.rs:1435-1437, 1534-1537, 1566`
- Inbound attachment writes use `let _ = std::fs::write(...)` / `let _ = std::fs::create_dir_all(...)`. A failed write still records `source_path: Some(...)` pointing at a nonexistent file, and the user's image silently vanishes.
- **Fix:** Log errors and surface failure to the agent/user.

#### [BLOCK] Session Store Non-Atomic Read-Modify-Write
- **Crate:** `rsclaw-store` — `redb_store.rs:455-488`
- `append_message` reads `message_count` outside the write transaction, then writes with a separately-computed `seq`. Two concurrent appends can compute the same `seq` and the second `insert` silently overwrites the first message.
- **Fix:** Compute `seq` inside the write transaction.

#### [BLOCK] `:` Collision in Composite Store Keys
- **Crate:** `rsclaw-store` — `redb_store.rs:36, 469, 471, 592`
- Store keys use `:` as separator but session keys (from channel/peer IDs like `telegram:12345`) also contain `:`. This creates key-space collisions where `format!("{session_key}:{seq:016}")` and `format!("archive:{session_key}:gen{generation}:{seq:016}")` ambiguously parse.
- **Fix:** Use a non-colliding separator (e.g., `\0`) or escape `:` in session keys.

### 2.3 Production Panics

#### [BLOCK] String Byte-Slice Panic in Workspace
- **Crate:** `rsclaw-agent` — `workspace.rs:276`
- `let mut s = content[..MAX_BYTES].to_owned()` where `MAX_BYTES = 65536` — byte-slices arbitrary AGENTS.md content. If a multi-byte char straddles byte 65536, this panics in `collect_ancestor_agents_md` (coding-profile prompt build).
- **Fix:** Use `is_char_boundary` snap-back, matching sibling code patterns.

#### [BLOCK] Session-Key Byte Slice on `/status` Path
- **Crate:** `rsclaw-agent` — `registry.rs:283`
- `&key[..20]` byte-slices a session key that can contain non-ASCII (Chinese display names in Feishu session keys). Panics in `format_status()`.
- **Fix:** Use `chars().take(20)` like the sibling code at line 322-327.

#### [BLOCK] TaskStore `expect()` in Startup
- **Crate:** `rsclaw-runtime` — `gateway/startup.rs:1000`
- `TaskStore::open(&path).expect("open A2A task store")` panics the process mid-startup on a corrupt/locked database. Every other fallible open in startup uses `match`/`unwrap_or_else`.
- **Fix:** Match the surrounding error style with `context()` propagation.

#### [BLOCK] Gemini `unwrap()` on JSON Map
- **Crate:** `rsclaw-provider` — `gemini.rs:152`
- `body["generationConfig"].as_object_mut().unwrap()` — the only genuinely unnecessary production `unwrap()` in the provider crate; relies on a literal constructed 4 lines above.
- **Fix:** Defensive `if let Some` or `expect("...")` with explanation.

### 2.4 Dead/Silently-Ignored Configuration

#### [BLOCK] RetryConfig `attempts` Field Dead — User Config Silently Ignored
- **Crate:** `rsclaw-provider` — `lib.rs:526-553`, `failover.rs:80`, `rsclaw-config schema.rs:1730-1735`
- The config schema's `retry` block (`attempts`, `minDelayMs`, `maxDelayMs`) has no effect on actual provider retries. `FailoverManager` hardcodes `TRANSIENT_RETRY_MAX = 2` and the crate's own `RetryConfig.attempts` field is unused.
- **Fix:** Wire `RetryConfig` into `FailoverManager` or document the config field as non-functional.

#### [BLOCK] `deny_unknown_fields` Doc/Behavior Mismatch
- **Crate:** `rsclaw-config` — `schema.rs:1-2`, `lib.rs:12`
- Module docs claim "Unknown fields cause deserialization to fail (deny_unknown_fields)" but only ONE struct (`KbOcrConfig`) has `#[serde(deny_unknown_fields)]`. All other ~40 structs silently accept typos like `gateway.portt` or `channels.telegram.botToken2`.
- **Fix:** Either apply `deny_unknown_fields` consistently or update the docs.

#### [BLOCK] Secret Resolution Stub — `File`/`Exec` Secrets Silently Drop to `None`
- **Crate:** `rsclaw-config` — `secrets.rs:20-41`, `runtime.rs:282-293`
- The secret-resolution pipeline for `File`/`Exec` refs is a documented stub. `Env` refs resolve; `File`/`Exec` return `None` silently. This means a `gateway.auth.token` configured as a file ref yields an unauthenticated gateway with zero error surfaced.
- **Fix:** At minimum, `warn!` when file/exec secrets are unresolved; surface the error or implement resolution.

### 2.5 Skills/Plugin Security

#### [BLOCK] Skills.sh Install Path Allows Directory Escape
- **Crate:** `rsclaw-skill` — `clawhub.rs:600-608`
- `install_from_skillsh` writes `install_dir.join(path)` where `path` comes from a remote `files[].path`. `Path::join` with `../` escapes. The zip/tarball extraction paths DO guard against this; the skills.sh direct-write path does not.
- **Fix:** Reuse the zip-slip component check or reject non-normal path components.

### 2.6 Missing Timeouts

#### [BLOCK] Gemini Provider Has No Request Timeout
- **Crate:** `rsclaw-provider` — `gemini.rs:74-88`
- `stream()` sets no `.timeout()` on the request. A stalled Gemini connection hangs the stream forever at the transport level. Anthropic has 120s; rsclaw has 45s read-idle + 60s header deadline.
- **Fix:** Add a transport timeout consistent with other providers.

### 2.7 Sensitive Data in Logs

#### [BLOCK] Anthropic Error Path Logs Full Request Body
- **Crate:** `rsclaw-provider` — `anthropic.rs:101-108`
- On non-2xx, `serde_json::to_string(&body).unwrap_or_default()` writes the full request body (full user conversation content) into `tracing::warn!` — sensitive-content exposure to logs.
- **Fix:** Truncate or redact the logged body, or log only metadata (status, model, error type).

### 2.8 Channel Silent Delivery Failures

#### [BLOCK] Chat-Mode Queue Messages Can Be Silently Dropped
- **Crate:** `rsclaw-runtime` — `gateway/task_queue.rs:1216-1266`
- For chat-mode tasks, direct-LLM fast paths in every channel await `reply_rx` with a 10s timeout then silently drop the reply. The task-queue path drops on full queue with only `try_send` → warn. A chat message during agent load can be acknowledged, never queued, and never answered with no error surfaced.
- **Fix:** Surface errors to the user or fall back to queuing.

---

## 3. [SUGGEST] Issues — Recommended Improvements

### 3.1 Architecture & Design

| # | Crate | File:Line | Issue |
|---|-------|-----------|-------|
| S1 | `rsclaw-agent` | `collaboration.rs:150` | `.leak()` per `dispatch_a2a` call — permanent memory growth on LLM-invoked hot path |
| S2 | `rsclaw-agent` | `spawner.rs:90-462` | ~370 lines of near-identical duplicated code between `spawn_agent_with_kind` and `replace_agent` |
| S3 | `rsclaw-agent` | `compaction.rs:147-784` | `compact_inner` is ~640 lines in one function — highest maintenance risk |
| S4 | `rsclaw-agent` | `runtime/agent_loop.rs` | Main loop spans ~1700 lines in one function |
| S5 | `rsclaw-agent` | `tools_web.rs:153-676` | `tool_web_search` ~520 lines with duplicated URL construction and provider-merge logic |
| S6 | `rsclaw-agent` | `exec_pool.rs` | `ExecPool::spawn()` (130 lines) is dead code; `max_concurrent` field never enforced |
| S7 | `rsclaw-agent` | `context_mgr.rs` | ~350 lines of `#[allow(dead_code)]` media-description functions |
| S8 | `rsclaw-agent` | `registry.rs:838-1050` | 11 `.expect("agent registry lock poisoned")` on `RwLock` — inconsistent with rest of crate's `if let Ok` pattern |
| S9 | `rsclaw-runtime` | `gateway/hot_reload.rs` | `restart_tx` broadcast has no subscribers; single-consumer `reload_rx` — hot-reload story thinner than docs claim |
| S10 | `rsclaw-runtime` | `gateway/hot_reload.rs:44-58` | `ConfigChange::AgentUpdated/ChannelUpdated/...` variants never constructed — dead enum surface |
| S11 | `rsclaw-channel` | Multiple | Duplicated `ffmpeg-extract-and-transcribe` helper in 5 channels; duplicated `is_text_file` in 7 channels |
| S12 | `rsclaw-channel` | `telegram.rs`, `discord.rs` | `send_with_preview` placeholder→edit streaming code is dead — never called |
| S13 | `rsclaw-channel` | `slack.rs`, `discord.rs` | Temp file paths use untrusted remote filenames directly — path traversal / collision risk |

### 3.2 Error Handling & Robustness

| # | Crate | File:Line | Issue |
|---|-------|-----------|-------|
| S14 | `rsclaw-agent` | `goal.rs:218,240` | Silent `store.delete` errors — failed goal-clear silently re-loops a goal session |
| S15 | `rsclaw-agent` | `runtime/run_turn.rs:1435-1566` | 6 `let _ = std::fs::*` sites silently discard file I/O errors on upload pipeline |
| S16 | `rsclaw-agent` | `runtime/mod.rs:1202` | Silent `remove_file` error on video-delete path |
| S17 | `rsclaw-runtime` | `gateway/task_queue.rs:645-652` | `unstage_file` returns empty data on read failure, indistinguishable from empty upload |
| S18 | `rsclaw-runtime` | `gateway/channels/*.rs` | `text[5..]` byte-slicing on CJK command prefixes — fragile to future prefix changes |
| S19 | `rsclaw-runtime` | `gateway/channels/custom.rs:418-425` | `send_reply` returns `Ok` after a warn when the reply webhook returns non-2xx |
| S20 | `rsclaw-channel` | `line.rs:313-321` | `send_image` swallows entire push error: `let _ = ...send().await;` — not even logged |
| S21 | `rsclaw-channel` | `wecom.rs:736-749` | `send_markdown` fire-and-forget via `ws_tx.send()` — errors only `error!`-logged; `send()` returns `Ok` |
| S22 | `rsclaw-channel` | `wecom.rs` | Outbound message buffer is unbounded; stuck messages delivered on reconnect could be stale |
| S23 | `rsclaw-provider` | `anthropic.rs:482`, `gemini.rs:300` | Unknown/missing-`type` SSE frames silently dropped (no log, no `Done`) — same class as the fastshot "empty completion" bug |
| S24 | `rsclaw-provider` | `openai.rs:336` | Chat-completions path has no headers-phase timeout (inconsistent with anthropic 120s, rsclaw 60s) |
| S25 | `rsclaw-provider` | `rsclaw.rs:682-684` | `RSCLAW_DUMP_TURN` gate uses `is_ok()` — empty-string env var still enables full-request dumps |
| S26 | `rsclaw-store` | `redb_store.rs:363-387` | `delete_session` range iteration uses `unwrap_or(false)` on storage errors — silently truncates deletion |
| S27 | `rsclaw-store` | `redb_store.rs:529-552` | Archive backfill `unwrap_or_default()` on serialization failure, and entire backfill is best-effort with `error!` only |
| S28 | `rsclaw-store` | `redb_store.rs` | Inconsistent corrupt-row policy: `dequeue_task` aborts on one bad row vs `delete_session` silently skips |

### 3.3 Testing Gaps

| # | Area | Issue |
|---|------|-------|
| S29 | `rsclaw-mcp` | Zero tests for the JSON-RPC client |
| S30 | `rsclaw-events` | Zero tests for event DTOs |
| S31 | `rsclaw-runtime a2a/store.rs` | Zero tests for task persistence, push-config CRUD, owner index |
| S32 | `rsclaw-runtime a2a/push.rs` | Zero tests (sign_payload HMAC, dispatcher retry/backoff untested) |
| S33 | `rsclaw-runtime a2a/streaming.rs` | Zero tests (relay-forward, ownership gate, synthetic-failure path) |
| S34 | `rsclaw-runtime hooks/` | Zero tests (token validation, mapping lookup, session-key prefix gate) |
| S35 | `ui/test/` | No tests for WS client (`rsclaw-ws.ts`), hooks, auth token resolution — the most complex frontend modules |
| S36 | `tests/provider_registry.rs:193-235` | 4 `#[ignore]`d fallback resolution tests — core production logic left untested |
| S37 | `rsclaw-provider gemini.rs` | No SSE parser tests (all other providers have them) |
| S38 | `rsclaw-skill runner.rs:79` | Missing `CREATE_NO_WINDOW` for skill tool spawns on Windows |
| S39 | `rsclaw-mcp lib.rs:74` | Missing `CREATE_NO_WINDOW` for MCP server spawn on Windows |

### 3.4 Channel i18n Gaps

| # | Crate | File | Issue |
|---|-------|------|-------|
| S40 | `rsclaw-channel` | `matrix.rs:392,398` | Hardcoded English `"[voice message - transcription failed]"` / `"[voice message received]"` — should use i18n |
| S41 | `rsclaw-channel` | `auth/mod.rs`, `auth/feishu_auth.rs`, `auth/dingtalk_auth.rs` | CLI login/QR flows print hardcoded English via `println!` instead of i18n |

### 3.5 Uncategorized

| # | Crate | File:Line | Issue |
|---|-------|-----------|-------|
| S42 | `rsclaw-runtime` | `ws/methods/system.rs:257-308` | WS `logs.tail` returns raw, unredacted log lines — HTTP `/api/v1/logs` redacts secrets |
| S43 | `rsclaw-runtime` | `ws/rate_limit.rs:10-47` | Per-connection rate limiter trivially defeated by reconnecting (fresh bucket each time) |
| S44 | `rsclaw-provider` | `gemini.rs:69-72` | URL built by direct string interpolation — a model ID containing `/` or `?` corrupts the URL |
| S45 | `rsclaw-i18n` | `lib.rs:2577-2588` | `t_fmt` JSON mode doesn't escape quotes/backslashes — a `"` in a translated value breaks the JSON |
| S46 | `rsclaw-heartbeat` | `state.rs:78-91` | `save()` is an unlocked read-modify-write of the whole state array — concurrent saves can lose updates |
| S47 | `rsclaw-a2a-types` | `types.rs:293` | `PushNotificationConfig` derives `Debug` with a `token` field — check no caller `{:?}`-logs it |
| S48 | `rsclaw-browser` | `pool.rs:435-467` | `wait_for_selector` swallows all errors — selector-wait can never report failure |

---

## 4. [NOTE] Observations — Non-Blocking

### 4.1 Architecture Notes

- **Channel DM policy enforcement** lives entirely in the runtime gateway wrappers, not in the channel crate — the crate merely provides `DmPolicyEnforcer` (in-memory by default). Only `custom` and `wecom` wire redb persistence for pairing approvals.
- **Per-user queue management** is consistent across all channels but lives in runtime, not the channel crate.
- **No 5-state WS state machine** exists server-side — only a flat connection registry (`HashMap<ConnId, Arc<RwLock<ConnHandle>>>`). The 5-state model is client-side only.
- **`provider_slot` hot-swap** is aspirational — no code path replaces providers post-startup, despite the "hot-swappable via rsclaw reload" doc comment.
- **Config-change hot-reload** `FileWatcher` pre-load parses errors silently with `.ok()`, making the first config-fix misclassified as hot-safe instead of restart-required.
- **Two parallel auth systems** coexist in the UI: chat access code (useAccessStore) vs. gateway bearer token (rsclaw-api).

### 4.2 Code Quality Notes

- **No `async-trait` usage** found anywhere in the codebase — clean Rust 2024 native async fn in traits throughout.
- **String truncation is overwhelmingly safe**: `char_indices()` / `chars().take()` / `truncate_str()` used consistently. The few exceptions are the [BLOCK] items above.
- **Doc comment coverage** is excellent — nearly every `pub fn` in core crates has a doc comment. Gaps are minor (CLI one-offs, errors.rs helpers).
- **`#[allow(dead_code)]` inventory** is ~1000+ lines across the agent crate: media-description functions, superseded plugin-tool renderers, `BuiltIn` regex handler variant, `builtin_overrides` empty map, `send_with_preview` streaming code, empty QQ Official Bot section.
- **Channel callback signatures** are inconsistent across channels (peer_id-only vs peer_id+chat_id vs chat_id-only, some pass images/files, some don't) — a maintenance burden but each is internally consistent.

### 4.3 CLI Notes

- **55-command flat root enum** with overlapping aliases (`start/stop/restart/reload` + `gateway` + `daemon`) — organization is the biggest UX issue.
- **Stringly-typed enums** at 6+ sites (shell/format/mode/direction) — `#[arg(value_enum)]` would give free clap validation.
- **Secrets on CLI**: `gateway run --token` and `qr --token` accept secrets as plain CLI args (visible in `ps`).
- **`--container` flag** is a documented stub ("Currently prints a warning") — dead UX surface.

### 4.4 KB Crate Notes

- **`delete_doc`/`set_doc_visibility`/`delete_doc_by_id`** are non-atomic (read-then-write in two transactions) unlike the `_in_wtx` ingest variants.
- **`UrlSyncer`** has no wiremock e2e test — the only syncer without one.
- **Slug generation** drops fullwidth CJK characters (`０１２` etc.) to dashes, causing cosmetic collisions (paths remain unique via hash suffix).
- **Entity canonical_id** uses 8-hex-hash (32-bit space) — collision risk is `~√(2^32)` ≈ 65k entities for 50% probability.

### 4.5 UI Notes

- **No Tailwind usage** — styling is SCSS modules + CSS vars per the AGENTS.md. However, ~1,000 inline `style={{}}` attributes and dozens of hardcoded hex colors undermine this system.
- **Hooks-before-early-returns** is consistently correct in all spot-checked components.
- **SSR safety**: `isTauriRuntime()` guard prevents Tauri API calls during server-side rendering.
- **`useRsClawSocket.ts` doesn't exist** — WS logic lives in `ui/app/lib/rsclaw-ws.ts` as a singleton class `RsClawWsClient`.
- **`layout.tsx`** hardcodes `<html lang="en">` while the app is zh/en bilingual.
- **Auth token priority** is correct: Tauri config > env > localStorage.

### 4.6 Test Notes

- **Rust integration tests**: ~58 files, 250+ test functions — strong coverage across channels, providers, A2A, KB, gateway auth.
- **Python e2e**: `a2a_e2e_runner.py` with 15-scenario live gateway matrix.
- **UI Jest**: 8 test files, but the most critical modules (WS client, auth, hooks) are untested; `sum-module.test.ts` is a placeholder.
- **11 `#[ignore]` tests**, including 4 stale provider-fallback pins and 3 `sse_stream` "flaky in CI".

---

## 5. Module-by-Module Summary

### 5.1 `rsclaw-agent` (51 files)
**Grade: B+**
- Clean Rust 2024 style (no async-trait), strong doc culture, thorough test coverage
- Main issues: 640-line `compact_inner`, 1700-line agent_loop, ~1000 lines dead code, 2 byte-slice panics, silent upload data loss
- Strengths: KV-cache-aware compaction, solid tool dispatch, deterministic loop detection

### 5.2 `rsclaw-runtime` (108 files)
**Grade: B**
- Largest and most complex crate; contains gateway, server, WS, A2A, cron, hooks
- Main issues: 3 auth bypasses, notification router Lagged exit, shutdown Notify race, A2A store auto-reset, `TaskStore::open().expect()`
- Strengths: Comprehensive shutdown coordinator, restart/respawn orchestration, A2A relay protocol with production-grade failover

### 5.3 `rsclaw-channel` (23 files)
**Grade: B+**
- All 13 channel drivers implement `Channel` trait correctly
- Main issues: duplicated ffmpeg/i18n helpers, silent send errors in custom/wecom/line, temp-file path traversal in slack/discord, hardcoded English in CLI QR flows
- Strengths: CJK-safe truncation throughout, consistent chunking with code-fence safety

### 5.4 `rsclaw-provider` (12 files)
**Grade: B+**
- Well-designed LlmProvider trait with solid failover/health state machine
- Main issues: dead `RetryConfig.attempts` (config silently ignored), Gemini no timeout, Anthropic error path logs full request body, Gemini `unwrap()`, missing SSE-frame-type handling
- Strengths: rsclaw provider is production-grade with 8-rule dispatch, replay recovery, poison-tolerant locking; thorough wiremock integration tests

### 5.5 `rsclaw-kb` (83 files)
**Grade: A-**
- Most disciplined crate in the codebase; zero production unwraps, zero unlogged error discards
- Main issues: non-atomic visibility/status operations, UrlSyncer lacks wiremock e2e, slug drops fullwidth CJK to dashes
- Strengths: Single-tx ingest with race re-check, deterministic chunk IDs, rebuildable-from-redb indexes, textbook RRF/MMR with deterministic tie-breaking, comprehensive tests

### 5.6 `rsclaw-config` (11 files)
**Grade: B**
- Main issues: `deny_unknown_fields` doc/behavior mismatch, secret resolution stub drops File/Exec to None silently, `OnceLock` proxy double-init panic risk, backup failure silently ignored
- Strengths: Well-documented pipeline, env-file 0600+atomic rename+fsync hygiene, `$include` traversal guard, deliberate design decisions with rationale comments

### 5.7 `rsclaw-store` (3 files)
**Grade: B-**
- Main issues: Non-atomic read-modify-write on session messages, `:` key collisions, silent error discards in hot paths, incomplete archive backfill
- Strengths: v2→v3 upgrade with backup+child-process isolation, lock-retry handoff, 17 focused tests

### 5.8 `rsclaw-cli` (37 files)
**Grade: B-**
- Main issues: Flat 55-command root enum, stringly-typed enums at 6+ sites, secrets on CLI args, undocumented variants/args in many files, `--container` dead stub
- Strengths: Clap 4 usage is idiomatic, `UpdateWrapper` optional-subcommand pattern correct, good doc comments on newer commands (skills, kb, memory)

### 5.9 Smaller Crates (cap, computer, skill, plugin, watch, browser, heartbeat, mcp, migrate, events, i18n, util, types, a2a-types)
**Grade: A-/B+**
- Main issues: Skills.sh path escape [BLOCK], missing `CREATE_NO_WINDOW` on MCP/skill spawns, 3 production-panic `expect`s in plugin wasm_runtime, zero tests for MCP client and events
- Strengths: Cap drivers are clean with documented retry/timeout; computer-use loop is model-agnostic with excellent defensive parsing; WASM sandbox is thorough (device-key 0o600, localhost block, SQL policy); `truncate_str` is canonical and well-tested

### 5.10 `ui/`
**Grade: C+**
- Main issues: ~1,000 inline styles + hardcoded hex colors, no WS state machine, WS client untested, no tests for critical hooks, layout hardcodes `lang="en"`
- Strengths: Hook ordering correct, auth token priority correct with incident-documented fix, error/loading/empty states well-implemented in KB panel, Tauri access centralized

### 5.11 `tests/`
**Grade: B+**
- Main issues: 11 ignored tests, 4 stale provider-fallback pins, UI Jest suite nearly vacuous, SSRF redaction missing from WS `logs.tail`
- Strengths: Extensive Rust integration suite with 58 files and 250+ tests; Python A2A e2e runner; KB pipeline week1-4 test progression

---

## 6. Top 10 Action Items (Priority Order)

1. **Fix WS auth bypass on `/` path** — unauthenticated root WS upgrade defeats configured gateway token
2. **Fix A2A TaskStore auto-reset** — transient errors wipe task history silently
3. **Fix shutdown Notify multi-waiter race** — late subscribers hang forever, preventing graceful drain
4. **Fix notification router `Lagged` exit** — one burst kills all notification fan-out permanently
5. **Fix session store non-atomic read-modify-write** — concurrent appends can silently overwrite messages
6. **Fix `:` key collisions in store** — real-world session keys collide with composite key format
7. **Fix skills.sh path escape** — remote-controlled path allows `../` directory traversal
8. **Fix dead RetryConfig** — user-configured retry knobs silently ignored
9. **Fix secret resolution stub** — File/Exec secret refs silently yield unauthenticated gateway
10. **Add Gemini request timeout** — stalled connections hang forever

---

*Generated by rscode multi-subagent review. Each finding is evidence-backed with file path, line number, and code quote in the detailed subagent reports. For full traceability, see the individual subagent outputs in the session.*
