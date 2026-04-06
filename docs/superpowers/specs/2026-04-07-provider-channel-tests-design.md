# Provider & Channel Comprehensive Test Design

## Overview

Full test coverage for the `provider` and `channel` modules, combining unit tests for core logic with wiremock-based integration tests for HTTP/WebSocket interactions.

**Scope:** ~260 new tests across 29 files (6 provider, 5 channel shared, 16 per-channel, 2 shared helpers).

## Architecture Context

- Multiple channels can bind to a single agent via `channels[]` declarations or `bindings[]` rules
- Agents can interact internally or externally via A2A protocol
- Channel layer is transport-only: it doesn't know about agent internals or A2A
- Routing priority: explicit bindings > channel declarations > default agent
- Session keys encode agent+channel+peer for isolation

## File Layout

```
tests/
  common/
    mod.rs                    # existing: free_addr, minimal_config, start_server
    mock_provider.rs          # NEW: SSE/JSONL mock builders
    mock_channel.rs           # NEW: channel wiremock/WS mock tools

  # Provider tests
  provider_mock.rs            # EXTEND: ContentPart, LlmRequest edge cases, RetryConfig
  failover_retry.rs           # EXTEND: alias fallback, non-retryable errors, profiles
  provider_registry.rs        # NEW: resolve_model full paths, alias priority, fallback chain
  provider_anthropic.rs       # NEW: wiremock SSE parsing
  provider_openai.rs          # NEW: wiremock SSE + non-SSE JSON fallback + think tags
  provider_openai_ollama.rs   # NEW: Ollama JSONL format, reasoning model routing
  provider_gemini.rs          # NEW: wiremock Gemini SSE

  # Channel shared logic
  dm_policy.rs                # EXTEND: TTL expiry, concurrency, persistence
  channel_chunker.rs          # NEW: fence protection, break cascade, unicode, platform limits
  channel_manager.rs          # NEW: tier limits, routing (multi-channel→agent, account match)
  channel_send_retry.rs       # NEW: retry logic, cross-channel reply
  channel_media.rs            # NEW: media detection, office extraction

  # Per-channel integration tests
  channel_telegram.rs         # NEW: long-poll, sendMessage, editMessageText, photo/voice
  channel_discord.rs          # NEW: Gateway WS, REST, identify/heartbeat, MESSAGE_CREATE
  channel_slack.rs            # NEW: Socket Mode, chat.postMessage, file upload
  channel_whatsapp.rs         # NEW: webhook, media upload 2-step, text/image/audio
  channel_signal.rs           # NEW: JSON-RPC stdio mock
  channel_qq.rs               # NEW: WS gateway, token refresh, group/c2c/guild
  channel_line.rs             # NEW: webhook, push API, image upload
  channel_zalo.rs             # NEW: webhook, OA API, image template
  channel_dingtalk.rs         # NEW: Stream Mode WS, token refresh, batch send
  channel_feishu.rs           # NEW: WS endpoint, tenant token, interactive card
  channel_wecom.rs            # NEW: WS RPC, 3-step chunked media upload
  channel_wechat.rs           # NEW: ilink API, AES-128-ECB, QR login, long-poll
  channel_matrix.rs           # NEW: Client-Server sync, media upload, mxc URLs
  channel_custom.rs           # NEW: webhook + WS modes, JSON path, template, auth
  channel_cli.rs              # NEW: stdin/stdout basic tests
  channel_transcription.rs    # NEW: Whisper API, SILK detection, TTS
```

## Shared Helpers

### `tests/common/mock_provider.rs`

Reusable SSE/JSONL mock response builders:

- **Anthropic SSE**: `AnthropicEvent` enum (TextDelta, ThinkingDelta, ToolUseStart, InputJsonDelta, MessageDelta, Error, Done) + `mount_anthropic_stream()`
- **OpenAI SSE**: `OpenAiEvent` enum (TextDelta, ReasoningDelta, ToolCallDelta, FinishReason, Done) + `mount_openai_stream()`, `mount_openai_json()`
- **Ollama JSONL**: `OllamaEvent` enum (Content, Thinking, ToolCall, Done) + `mount_ollama_native()` — NOT SSE, one JSON object per line
- **Gemini SSE**: `GeminiEvent` enum (TextPart, FunctionCall, FinishReason) + `mount_gemini_stream()`
- **Stream assertions**: `collect_stream_events()`, `assert_stream_text()`, `assert_stream_tool_call()`, `assert_stream_done()`, `assert_usage()`

### `tests/common/mock_channel.rs`

- **OutboundMessage builders**: `text_msg()`, `group_msg()`, `msg_with_reply()`, `msg_with_images()`
- **HTTP mocks**: `mount_ok_json()`, `mount_rate_limit()`, `mount_error()`, `mount_token_refresh()`
- **Chunked send assertions**: `assert_chunked_sends()` — extracts text from received requests via JSON path
- **WebSocket mock**: `MockWsServer` with scripted `WsAction` enum (Send, Receive, ReceiveAndReply, Sleep, Close)

## Provider Tests Detail

### provider_mock.rs (EXTEND existing 7 tests)

New tests (~15):
- ContentPart serialization: Text, Image, ToolUse, ToolResult (with is_error)
- MessageContent variants: Text vs Parts
- LlmRequest: with tools, with thinking_budget, clone independence
- StreamEvent construction: TextDelta, ToolCall, Done with/without usage
- RetryConfig: defaults, partial deserialization

### provider_registry.rs (NEW ~20 tests)

- `resolve_model` all paths: explicit prefix, inferred (anthropic/gemini/deepseek/qwen/zhipu/kimi/stepfun/xai/openai default)
- Alias priority: alias overrides inference, alias target not registered falls through
- Fallback chain: custom > ollama > first registered > raw inference
- Registry CRUD: register/get/names, unregistered returns error
- Edge cases: no providers registered, multiple slashes in model string

### provider_anthropic.rs (NEW ~12 tests, wiremock)

All use `AnthropicProvider::with_base_url(server.uri(), "test-key")`:
- Stream parsing: text deltas, tool use (content_block_start + input_json_delta), thinking discarded, error event, [DONE] marker
- Error handling: HTTP 401/429/500, malformed JSON in SSE, empty body
- Request format: headers (x-api-key, anthropic-version), body structure, thinking_budget mapping

### provider_openai.rs (NEW ~12 tests, wiremock)

All use `OpenAiProvider::with_base_url(server.uri(), "test-key")`:
- SSE: text delta, tool_call delta accumulation, reasoning_content with `<think>` tag wrapping (thread_local state), reasoning-only stream
- Non-SSE fallback: JSON response → [TextDelta, Done]
- Think tag stripping: complete, unclosed, empty, no tags, lone closing
- Errors: HTTP errors, malformed SSE

### provider_openai_ollama.rs (NEW ~12 tests, wiremock)

All use `OpenAiProvider::ollama(server.uri(), None)`:
- Model routing: qwen3/qwq/deepseek-r1 → `/api/chat`; other models → `/v1/chat/completions`
- JSONL parsing: content stream, thinking with `<think>` tags (Arc<AtomicBool>), thinking-only done closes tag, tool call (JSON Object arguments, id="call_{name}"), mixed thinking+tool
- Request format: model, stream=true, think=false, options, tools
- Errors: HTTP error, malformed JSONL, empty response

### provider_gemini.rs (NEW ~8 tests, wiremock)

All use `GeminiProvider::with_base_url(server.uri(), "test-key")`:
- SSE: text parts, function call (id==name), multiple parts in one event, error field
- Request format: URL includes model name, Gemini native body (contents/systemInstruction/tools), API key in query params
- Errors: HTTP errors

### failover_retry.rs (EXTEND existing 4 tests, ~12 new)

- Alias + failover: alias provider fails, fallback succeeds
- Non-retryable error: 500 propagated immediately, fallback not called
- Profile rotation: multiple profiles tried in order
- Provider not in registry: skipped gracefully
- Cooldown bounds: min 5s, max 300s
- Error classification: rate limit variants (429, "rate limit", "too many requests"), auth variants (401, "unauthorized", "invalid api key"), non-matching variants
- Edge cases: empty fallback list, single provider with multiple profiles

## Channel Shared Logic Tests Detail

### channel_chunker.rs (NEW ~16 tests)

- Fence protection: no split inside code block, fence reopened on forced split, language tag preserved, nested backticks, multiple fences
- Break preference cascade: paragraph → newline → sentence → whitespace → hard
- Platform limits: all 14 platforms verified
- Edge cases: exact limit, one over limit, unicode CJK, emoji boundary, min_chars, total content preservation

### dm_policy.rs (EXTEND existing 8 tests, ~10 new)

- TTL: expired code rejected, expired entries cleaned on list_pending, freed slot allows new pairing
- Concurrency: concurrent pairing requests (≤ MAX_PENDING), concurrent approve same code (exactly one succeeds)
- Persistence: survives restart with RedbStore, revoke removes from persistence
- Edge cases: case-insensitive code, same peer reuses code, code character set validation, empty allowlist, empty peer_id

### channel_manager.rs (NEW ~10 tests)

- Tier limits: Low=3, Standard=8, High=unlimited
- CRUD: register/get, duplicate name overwrites
- Multi-channel routing: multiple channels → same agent, unbound → default, account exact match priority, bare vs account match, binding priority override, alphabetical tiebreak

### channel_send_retry.rs (NEW ~6 tests)

Uses `CountingChannel` mock implementing `Channel` trait:
- Retry: succeeds on second attempt, exhausted returns last error, first attempt succeeds
- Delay: increases between attempts
- Edge: single attempt mode
- Cross-channel: same agent replies via different channels

### channel_media.rs (NEW ~12 tests)

- Image detection: by MIME (jpeg/png/gif/webp/svg+xml), by extension (12 formats + uppercase), negative cases
- Audio detection: by MIME (audio/*, voice), by extension (10 formats)
- Video detection: by MIME (video/*), by extension (8 formats)
- Office extraction: minimal docx/xlsx/pptx ZIP → text extraction, unsupported extension → None, corrupt ZIP → None

## Per-Channel Integration Tests Detail

### channel_telegram.rs (~15 tests)
- Send: text, with reply_to, with thread_id, chunked at 4096, image multipart, text+images
- Errors: HTTP error, 429 rate limit
- Preview: placeholder → edit
- Run (getUpdates): text, group, voice, photo, document, skip bot, reconnect

### channel_discord.rs (~10 tests)
- Send: text, chunked at 2000, image multipart, 429 retry
- Preview: POST then PATCH
- Gateway: identify+heartbeat, MESSAGE_CREATE dispatch, ignore bot, attachments, reconnect

### channel_slack.rs (~8 tests)
- Send: text, chunked at 3000, image upload
- Socket Mode: connect, events_api ACK, files, disconnect reconnect, ignore non-message

### channel_whatsapp.rs (~10 tests)
- Send: text, chunked at 4000, image upload (2-step: media upload → send)
- Webhook: text, image, audio, document, empty ignored, multiple messages
- Media download: 2-step (get URL → download), error handling

### channel_signal.rs (~6 tests)
- Send: direct (JSON-RPC send), group (sendGroupMessage), image attachment
- Receive: data message, group message, empty ignored

### channel_qq.rs (~10 tests)
- Token: refresh on startup, refresh before expiry
- Send: group, c2c, guild channel, chunked at 4096, image upload
- Gateway: identify, GROUP_AT_MESSAGE, C2C_MESSAGE, heartbeat

### channel_line.rs (~7 tests)
- Send: push message, chunked at 5000, image upload
- Webhook: text event, group message, image, audio

### channel_zalo.rs (~6 tests)
- Send: text, chunked at 2000, image upload (attachment_id template)
- Webhook: text, image, audio, file

### channel_dingtalk.rs (~8 tests)
- Token: refresh
- Send: to user (batch API), to group, chunked at 20000, image upload
- Stream Mode: connect with ticket, message dispatch + ACK, ping/pong, group message

### channel_feishu.rs (~8 tests)
- Token: tenant token refresh
- Send: interactive card, reply to message, chunked at 4000
- WebSocket: endpoint connect, im.message.receive, bot ignored, audio transcription, pong ignored, reconnect

### channel_wecom.rs (~8 tests)
- Auth: subscribe frame, auth failure
- Send: markdown, chunked at 4096
- Media: 3-step RPC upload (init→chunks→finish), large image multiple chunks
- Receive: aibot_msg_callback
- Heartbeat, reconnect

### channel_wechat.rs (~12 tests)
- Send: text message (ilink API headers)
- Run: getupdates poll, image item, voice item (prefer builtin STT), skip bot messages
- AES: ECB roundtrip, PKCS7 padding, hex-to-bytes
- URL encoding: special chars, unreserved preserved
- QR login: full flow, expired

### channel_matrix.rs (~7 tests)
- Send: text (PUT), chunked at 10000, image upload (mxc URI)
- Sync: text message, ignore own, image event, next_batch tracking

### channel_custom.rs (~15 tests)
- JSON path: deep nesting, array OOB, null, number, empty
- Template: all variables, missing preserved, special char escaping
- Webhook: POST/PUT, custom headers, env expansion, inbound parsing, filter reject
- WebSocket: connect+receive, auth frame, heartbeat, send reply, reconnect

### channel_cli.rs (~5 tests)
- Send: stdout, ignore images, no chunking
- Constants: name, peer_id

### channel_transcription.rs (~6 tests)
- OpenAI Whisper: success, error
- SILK: format detection (v1, v3)
- Provider detection: env var priority, fallback
- TTS: OpenAI speech API
