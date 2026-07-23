# Review: dev
Date: 2026-07-23

## Summary

Reviewed all changes from `6b9dd6a9fe03f0e22fc1817f1189132d1c0534f7` through `be66c3e3`. `RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo test --workspace -q` passes, but the range contains release-publishing, security, hot-reload lifecycle, and OCR defects that are not covered by that suite.

## Issues

### [BLOCK] Publishing reuses stale internal-crate versions
File: scripts/cargo-publish.sh:66

Every internal crate still declares `version = "0.1.0"` (for example `crates/rsclaw-agent/Cargo.toml:3`), while the script treats Cargo's `already exists` upload failure as success. `cargo search` confirms `rsclaw-agent = 0.1.0` and `rsclaw-provider = 0.1.0` already exist. Consequently a release publishes `rsclaw-runtime 2026.6.26` against old crates.io artifacts rather than this range's source. Version all changed publishable crates for each release, and only accept an existing version after verifying its checksum/source matches.

### [BLOCK] Open gateway trusts a client-forged forwarding header
File: crates/rsclaw-runtime/src/server/mod.rs:3597

When gateway auth is disabled, `trusted_headers` is decided from the request's `X-Forwarded-For`, which an external client can supply as `127.0.0.1`. The following code then accepts attacker-controlled `X-Session-Key`, `X-User-Id`, and `X-Channel` at lines 3651-3705, permitting session-context spoofing. Derive locality from `ConnectInfo<SocketAddr>`; parse forwarded headers only behind an explicitly trusted proxy.

### [BLOCK] Custom Responses and Ollama providers still leak `OPENAI_API_KEY`
File: crates/rsclaw-provider/src/build.rs:141

The Ollama and OpenAI Responses branches unconditionally fall back to `OPENAI_API_KEY` even when `baseUrl` points to a custom endpoint. `OpenAiProvider` attaches that value as `Authorization: Bearer` (`openai.rs:764-766`). A provider configured with `api: "openai-responses"`, a third-party base URL, and no `apiKey` therefore receives the gateway's OpenAI credential. Apply the same custom-endpoint guard used by the completions branch.

### [BLOCK] CLI image output writes to a predictable shared temporary path
File: crates/rsclaw-channel/src/cli.rs:55

Generated images are always written beneath `/tmp/rsclaw-cli-images/image_{i}{ext}`. The directory creation error is discarded and `tokio::fs::write` follows symlinks, allowing a local attacker to pre-create a symlink and redirect/overwrite the predictable output. The default directory/file permissions also expose generated images to other local users. Use a private randomly-named temporary directory and create output files without following symlinks.

### [BLOCK] HTTP rate limiter leaks one map entry per observed IP
File: crates/rsclaw-runtime/src/server/mod.rs:93

`map.entry(ip).or_default()` allocates an entry for every source address, while cleanup only retains entries inside the current address's vector and never removes empty or expired map keys. Requests from many IPv6/proxy addresses grow the map permanently until restart. Bound the key space and evict expired idle keys (for example with a TTL/LRU cache).

### [BLOCK] Reload removes the implicit default `main` agent
File: crates/rsclaw-runtime/src/server/mod.rs:2111

With `agents.defaults` but no `agents.list`, startup synthesizes `main`, whereas reload converts the absent list into an empty set and removes every running agent at lines 2175-2186. The registry's `default_id` remains `main`, so subsequent dispatch fails with `agent not found: main`. Preserve/reconstruct the synthesized default entry during reload and add a regression test.

### [BLOCK] Channel reload removes custom webhook channels but retains their routes
File: crates/rsclaw-runtime/src/server/mod.rs:1981

The configured-name list includes only built-in channel types. A configured custom channel such as `orders-hook` is therefore unregistered at lines 2009-2019. Its webhook remains in `custom_webhooks` (`gateway/channels/custom.rs:408-413`), so `/hooks/<name>` continues returning 202 while its cancelled outbound consumer discards replies. Diff custom channel names and remove/rebuild their webhook registrations atomically.

### [BLOCK] Custom WebSocket tasks survive channel removal
File: crates/rsclaw-runtime/src/gateway/channels/custom.rs:782

`ChannelManager::unregister` only cancels tokens registered in `ChannelManager` (`rsclaw-channel/src/lib.rs:742-747`). `start_custom_websocket` registers no token and its task selections observe only global shutdown, so a removed custom WebSocket stays connected until process exit. Register the channel token and select on it in both the receive and outbound tasks.

### [BLOCK] Multi-account channel cancellation only stops the final account
File: crates/rsclaw-runtime/src/gateway/channels/discord.rs:511

Discord registers its cancellation token under `DiscordChannel::name()`, which is always `"discord"` (`rsclaw-channel/src/discord.rs:623-626`). Each account overwrites the prior token in the token map, so removal cancels only the final account. The same bare-name pattern exists in Slack and the other multi-account channel starters. Key tokens by the registration/account name and cancel every alias belonging to the removed channel.

### [BLOCK] Concurrent reload requests can double-start a channel permanently
File: crates/rsclaw-runtime/src/server/mod.rs:2004

Each reload snapshots `running` before starting channels. Two simultaneous hot-adds both see the channel as absent, then each calls a starter; the later `register_with_name` and token insertion overwrite the earlier entries (`rsclaw-channel/src/lib.rs:665-671,709-722`). Later removal can only cancel the second task set. Serialize reload transactions or make registration/token creation an atomic check-and-insert operation.

### [BLOCK] WeChat 4.x Windows OCR can capture the foreground application
File: crates/rsclaw-desktop/src/native.rs:442

The new Alt+A path maps the normal `com.tencent.xinWeChat` input to process name `WeChat`, but WeChat 4.x uses `Weixin.exe`. On failure it substitutes the arbitrary rectangle `0,0,1200,800` and sends Alt+A/Enter to the current foreground app. This can OCR another application's clipboard image. Match both WeChat/Weixin process aliases and fail closed if no matching window is found.

### [BLOCK] Concurrent macOS OCR requests overwrite a process-global temporary image
File: crates/rsclaw-desktop/src/native.rs:687

The new path uses `/tmp/rsclaw_ocr_<pid>.png`; `DesktopSession::ocr_window` runs OCR work concurrently without a mutex. Two calls can overwrite each other's source image or delete it before the other call reads it. Use a secure per-call tempfile/UUID and a cleanup guard.

### [BLOCK] OCR CLI advertises model and language overrides that are ignored
File: crates/rsclaw-runtime/src/cmd/image.rs:27

`ImageOcrArgs` exposes `--model` and `--lang` (`rsclaw-cli/src/image.rs:39-45`), but the execution path passes only `path`, `prompt`, and `max_tokens` to `OcrClient`. `rsclaw image ocr image.png --model foo --lang ja` silently uses configured values instead. Implement per-call model/language overrides or remove these options.

### [BLOCK] Agent OCR silently truncates out-of-range `max_tokens`
File: crates/rsclaw-agent/src/tools_ocr.rs:56

The JSON `u64` is cast with `as u32`; `4294967296` becomes zero and is sent to the OCR service. Reject values above `u32::MAX` with `u32::try_from` and declare the same limit in the tool schema.

## Verdict

BLOCKED — 14 blocking issues must be resolved.

---

## Re-review: 2026-07-24

Re-reviewed the 14 items against `88fe1c66` plus the current uncommitted hot-reload changes. The targeted runtime suite passes (`cargo test -p rsclaw-runtime --lib -q`: 159 passed, 1 ignored), but `cargo test --workspace -q` currently cannot compile because multiple workspace crates lack their direct test dependencies (`tempfile`, `rustls`) and `rsclaw-heartbeat` imports a nonexistent `crate::MemoryTier`.

### Resolved / addressed

- Implicit `main` agent removal: addressed in `server/mod.rs:2221-2259`, which now synthesizes the same default agent used at startup.
- Custom webhook removal: addressed by including configured custom names and removing stale webhook entries during teardown (`server/mod.rs:2033-2040, 2061-2065`).
- Custom WebSocket cancellation: addressed by registering and selecting on the per-channel token (`gateway/channels/custom.rs:784-829`).
- Concurrent reload double-start: addressed by the gateway-wide reload mutex (`server/mod.rs:257-259, 1847-1848`).

### [BLOCK] Multi-account channel teardown still leaves a dead bare sender
File: crates/rsclaw-runtime/src/server/mod.rs:2056

The account-keyed cancellation change stops all account tasks, but channel removal only deletes `channel_senders[name]`, where `name` is e.g. `discord/default`. It leaves the bare fallback `discord` sender installed. Starters deliberately preserve that fallback with `entry("discord").or_insert_with(...)` (`gateway/channels/discord.rs:95-98`), so disabling then re-enabling Discord leaves account-less notifications routed to the old, cancelled sender. Remove/rebuild the bare fallback when the final account for a channel is removed.

### Still blocking (unchanged)

- Publishing stale internal crate versions: `scripts/cargo-publish.sh:66`.
- Forged `X-Forwarded-For` trusts OpenAI session headers: `server/mod.rs:3731-3755`.
- `OPENAI_API_KEY` leaks to custom Responses/Ollama endpoints: `provider/build.rs:141-150`.
- CLI image output uses predictable shared temporary paths: `channel/cli.rs:55-63`.
- Per-IP HTTP rate limiter retains every historical IP key: `server/mod.rs:93-100`.
- Windows WeChat 4.x OCR can capture the foreground application: `desktop/native.rs:442`.
- macOS OCR shares a PID-only temporary image: `desktop/native.rs:687`.
- `rsclaw image ocr --model/--lang` options remain ignored: `runtime/cmd/image.rs:27-33`.
- Agent OCR still truncates oversized `max_tokens`: `agent/tools_ocr.rs:56`.

## Re-review Verdict

BLOCKED — 10 blocking issues remain. The custom-channel and multi-account fixes are currently uncommitted.
