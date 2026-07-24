# Code Review — RsClaw (dev branch)

**Date:** 2026-07-24
**Scope:** Full codebase review (~206k lines across 17 crates)
**Commits reviewed:** Last 30 commits (hot-reload feature + supporting changes)

---

## Summary

| Severity | Count |
|----------|-------|
| **[BLOCK]** | 11 |
| **[SUGGEST]** | 16 |
| **[NOTE]** | 6 (highlights only) |

---

## [BLOCK] Items — Must fix before merge

### B1. `unwrap()` in AgentSpawner config read
**File:** `crates/rsclaw-agent/src/spawner.rs:106`
```rust
.unwrap_or_else(|_| Arc::clone(&*self.config.read().unwrap()));
```
If the `RwLock` is poisoned, the fallback calls `.unwrap()` again — double panic. Replace with `?` or `.expect("config rwlock poisoned")`.

### B2. `unwrap()` in MIME type parsing
**File:** `crates/rsclaw-runtime/src/server/mod.rs:5374`
```rust
headers.insert(header::CONTENT_TYPE, ct.parse().unwrap());
```
Replace with `ct.parse().unwrap_or_else(|_| mime::APPLICATION_OCTET_STREAM)`.

### B3. `unwrap()` in cron job state access
**File:** `crates/rsclaw-runtime/src/cron/mod.rs:315`
```rust
let state = job.state.as_mut().unwrap();
```
Replace with `.expect("state initialized above")`.

### B4. `unwrap()` in browser tool dispatch (3 sites)
**File:** `crates/rsclaw-agent/src/tools_web.rs:2399, 2538, 2552`
```rust
browser.as_mut().unwrap()
```
Replace with `.expect("browser initialized — checked above")`.

### B5. `unwrap()` in file write tool (2 sites)
**File:** `crates/rsclaw-agent/src/tools_file.rs:945-946`
```rust
let path = path.unwrap().to_owned();
let content = content.unwrap();
```
Replace with `.expect("path/content verified non-none above")`.

### B6. Silent error discard on session delete
**File:** `crates/rsclaw-agent/src/runtime/run_turn.rs:302`
```rust
let _ = self.store.db.delete_session(&key);
```
Fix:
```rust
if let Err(e) = self.store.db.delete_session(&key) {
    tracing::warn!(session = %key, "failed to delete session: {e}");
}
```

### B7. Silent error discard on webhook removal during hot-reload
**File:** `crates/rsclaw-runtime/src/server/mod.rs:2063-2065`
```rust
state.custom_webhooks.write()
    .map(|mut wh| { wh.remove(name); })
    .ok();
```
If the `RwLock` is poisoned, the webhook entry silently remains, routing to a cancelled channel. Log the error with `warn!`.

### B8. Hardcoded English user-facing strings in Feishu (3 strings)
**File:** `crates/rsclaw-channel/src/feishu.rs:1141`
```
"__DIRECT_REPLY__Video download failed (timeout or connection issue)..."
```
**File:** `crates/rsclaw-channel/src/feishu.rs:1239`
```
"__DIRECT_REPLY__File too large ({actual} MB, limit {limit} MB)..."
```
**File:** `crates/rsclaw-channel/src/feishu.rs:1250`
```
"__DIRECT_REPLY__File download failed (timeout or connection issue)..."
```
All three must go through `rsclaw_i18n::t_fmt()` with new i18n keys.

### B9. Hardcoded Chinese in Feishu notifier
**File:** `crates/rsclaw-channel/src/feishu.rs:2053`
```rust
"**[阅后即焚]**\n\n..."
```
Must use an i18n key (e.g. `burn_after_read_label`).

### B10. Missing doc comments on `pub fn` (multiple files)

| File | Functions missing `///` |
|------|------------------------|
| `crates/rsclaw-runtime/src/server/mod.rs:335` | `build_router()` |
| `crates/rsclaw-agent/src/loop_detection.rs` | `new`, `with_dual_thresholds`, `with_overrides`, `from_single_threshold`, `check_with_params`, `check`, `record_result`, `reset`, `last_result_hash` (9 functions) |
| `crates/rsclaw-agent/src/runtime/mod.rs:773` | `AgentRuntime::new()` |
| `crates/rsclaw-channel/src/lib.rs` | `ChannelManager::new/max_concurrent/register/get`, `PairingStore::new/is_approved/revoke`, `DmPolicyEnforcer::new/check/approve_pairing/revoke` (11 functions) |
| `crates/rsclaw-channel/src/feishu.rs` | `FeishuChannel::new()`, `FeishuNotifier::new()` |

### B11. Agent reload race — in-flight messages lost
**File:** `crates/rsclaw-runtime/src/server/mod.rs:2285-2297`

During agent model/prompt reload, the handler removes the old agent handle then spawns a new one. Between remove and spawn, messages routed to that agent ID get "agent not found" and are dropped.

**Fix:** Register the new handle before removing the old one, or make the swap atomic.

---

## [SUGGEST] Items — Recommended improvements

### S1. Non-atomic MCP reload — clear-then-respawn loses servers on failure
**File:** `crates/rsclaw-runtime/src/server/mod.rs:1994-2008`

`clients.clear()` runs before `respawn_mcp_servers()`. If respawn fails, all MCP tools are lost. Build new set first, then swap.

### S2. `config_reload` endpoint validates but discards result
**File:** `crates/rsclaw-runtime/src/server/mod.rs:1817-1829`

Loads config, checks if it parses, then throws it away. Rename to `config_validate` or wire it to apply.

### S3. CORS `permissive()` applied globally
**File:** `crates/rsclaw-runtime/src/server/mod.rs:514`

`CorsLayer::permissive()` disables CORS protection. Should be restricted when not binding to loopback.

### S4. `server/mod.rs` needs splitting (5749 lines)
Largest single file. Suggest: `routes/` module, `middleware.rs`, `state.rs`.

### S5. SSE parser duplication between rsclaw.rs and openai.rs
Both implement UTF-8 stitching, line buffering, and chunk assembly. Extract shared `SseParser`.

### S6. `OpenAiProvider` constructor proliferation (7 constructors)
Migrate to builder pattern (already has a TODO).

### S7. Archive entries never purged
`delete_session` preserves archive data indefinitely. Add `purge_archive` or retention policy.

### S8. HNSW index doesn't compact deleted nodes
Deleted docs remain as dead nodes in the graph. Periodic rebuild when deletion ratio is high.

### S9. Weakly-typed `Option<Value>` fields in config schema
`wizard`, `diagnostics`, `acp`, `node_host`, `broadcast`, `audio`, `media`, `discovery` bypass validation. Document which are passthrough.

### S10. `config schema` claims `deny_unknown_fields` but doesn't implement it
Module doc says unknown fields cause deserialization to fail, but the attribute is not present.

### S11. Browser idle reaper task has no cancellation handle
**File:** `crates/rsclaw-agent/src/runtime/mod.rs`

Background task spawned in `AgentRuntime::new()` with no `JoinHandle` stored. Store and abort in `Drop`.

### S12. File write failures silently discarded in tool cache
**File:** `crates/rsclaw-agent/src/runtime/run_turn.rs:1433-1564`

`let _ = std::fs::create_dir_all(...)` and `let _ = std::fs::write(...)` — log on failure.

### S13. WeChat QR login `println!` should use i18n
**File:** `crates/rsclaw-channel/src/wechat.rs:375, 410`

CLI-facing progress messages should use `rsclaw_i18n::t()` with `default_lang()`.

### S14. WASM `eval_with_args` uses string interpolation for JS construction
**File:** `crates/rsclaw-plugin/src/wasm_runtime.rs:549-572`

Fragile pattern — malformed `code` could break out of function wrapper. Consider more robust isolation.

### S15. Route lease validation is minimal in A2A relay
**File:** `crates/rsclaw-runtime/src/a2a/relay.rs:290-321`

Only checks `agent_ref` starts with `node_id/`. Consider restricting to a configured allowlist.

### S16. `list_pairings` does full table scan
**File:** `crates/rsclaw-store/src/redb_store.rs:664-681`

Use prefix range query instead. (Already has a TODO.)

---

## [NOTE] Highlights — Well-designed areas

### N1. Provider layer is clean
Zero BLOCK findings. Thorough error handling, proper credential management (SecretOrString everywhere), correct streaming with UTF-8 boundary handling, sound multi-layer failover (transport -> per-model -> chain), and comprehensive test coverage.

### N2. CancellationToken pattern is correctly implemented
All 13 channels register cancel tokens and `tokio::select!` on `cancel_token.cancelled()` in their run loops. Hot-reload removal properly fires the token to stop channel tasks.

### N3. Shutdown/restart endpoints are loopback-protected
`/api/v1/shutdown` and `/api/v1/restart` both check `is_loopback(peer)` and return 403 for non-loopback connections. Good security posture.

### N4. ShutdownCoordinator is well-designed
Double-check pattern, RAII `InflightGuard`, proper atomic flag separation. SSE streams use `take_until` for clean termination.

### N5. Compaction correctness
Three-mode dispatch (kv_cache_mode 0/1/2), layered head+summary+tail, deterministic entity extraction, iterative summary updates. Well-tested.

### N6. A2A relay security
Constant-time token verification, Ed25519 challenge-response, scope-based ACL, stream lifetime cap (30 min), RAII guard cleanup.

---

## Priority Fix Order

| Priority | Items | Effort |
|----------|-------|--------|
| **P0 — Fix now** | B1 (spawner panic), B11 (agent reload race) | Small |
| **P1 — Fix today** | B2-B5 (unwrap→expect, 8 sites), B6-B7 (silent discard), B8-B9 (i18n strings) | Medium |
| **P2 — Fix this week** | B10 (doc comments, ~25 functions), S1-S3 (MCP atomicity, config_reload, CORS) | Medium |
| **P3 — Backlog** | S4-S16 (refactoring, improvements) | Large |

---

## Overall Assessment

The codebase demonstrates strong engineering practices. The provider layer, shutdown coordinator, task queue, hot-reload cancellation, compaction, and A2A relay are all well-designed with proper concurrency primitives. The 11 BLOCK items are mostly mechanical (unwrap→expect, add doc comments, i18n strings) with one architectural fix (agent reload race). None indicate fundamental design problems.
