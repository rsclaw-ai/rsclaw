# ADR: Configuration Hot-Reload

Date: 2026-04-25
Status: Proposed
Decision: Phased implementation, Provider first

## Goal

Make all rsclaw.json5 configuration changes take effect without restarting the gateway.

## Trigger Mechanism

- File watcher (notify crate) + 2s debounce
- Manual `/reload` command
- HTTP/WS API for UI-driven changes
- Mutex to prevent concurrent reloads
- Parse failure = ignore, log warning

### Input Sources

- **File watch**: external editor / git checkout. Triggers full diff-apply.
- **HTTP/WS API**: UI panel edits. Apply in-memory THEN write file with a
  short-lived marker (`.rsclaw_reload_lock`) the watcher checks; watcher
  skips when marker present and removes it after expiry. Prevents the
  in-memory-write -> file-watch -> re-apply echo loop.
- **Manual `/reload`**: forces re-read from file regardless of watcher state.

API endpoint must be gated behind the same admin auth as `/api/admin/*`.

## Current Architecture Analysis

| Subsystem | Held As | Mutability | Hot-Reload Difficulty |
|-----------|---------|------------|----------------------|
| ProviderRegistry | `Arc<ProviderRegistry>`, internal HashMap, no interior mutability | Full Arc swap needed | Medium |
| AgentRegistry | `Arc<AgentRegistry>`, internal `RwLock<RegistryInner>` | Can insert/remove, but no cancel for running tasks | Hard |
| Channels | Each channel `tokio::spawn`'d, no JoinHandle/CancellationToken stored | Can't stop after startup | Very Hard |
| channel_senders | `Arc<RwLock<HashMap<String, mpsc::Sender>>>` | Already supports dynamic add/remove | Easy |

## Provider Hot-Reload Pitfalls

1. **Interior mutability needed**: `providers: HashMap` must become `RwLock<HashMap>` or `ArcSwap<HashMap>`. All `get()` call sites must clone `Arc<dyn LlmProvider>` inside the lock.

2. **In-flight streams hold old Arc**: Removing a provider can't reclaim connections immediately. Define: "remove = stop accepting new requests, old ones run to completion."

3. **Failover state migration**: `src/provider/failover.rs` maintains cooldown/failure counts per provider. On same-name replace: carry over state or reset? Both valid, must decide explicitly.

4. **Credential in-place update semantics**: Same-name update (change API key) = replace Arc. Old streams keep old key until completion. If OAuth refresh tokens stored in provider state, must migrate explicitly.

5. **Default provider deletion**: Bare model names resolve via default. See Decision: refuse removal of currently-default provider, require user to designate a new default first.

6. **AgentHandle.providers visibility**: Two routes possible:
   - **Interior `RwLock<HashMap>`** (chosen, see Decision): agents share the same `Arc<ProviderRegistry>`; mutations to the inner map are visible to all holders without changing the AgentHandle field. Simpler, no AgentHandle changes.
   - **`ArcSwap<ProviderRegistry>` whole-replace**: would require `AgentHandle.providers: Arc<ArcSwap<...>>` plus a `.load()` on every LLM call. Atomic snapshot semantics, but more invasive and not needed for our use case.

7. **In-flight call vs revoked credential**: rotating a leaked API key, the user expects "effective immediately." But existing streams hold an `Arc<Provider>` and finish on the old key. For now: documented grace period. If needed later, add `revoked: AtomicBool` per provider, checked between stream chunks.

## Agent Hot-Reload Pitfalls

1. **No CancellationToken**: Currently stopping a runtime only works by dropping `mpsc::Sender` (recv returns None). But tx is Arc::clone'd by channel handlers, A2A, WS, Spawner -- registry's own drop doesn't stop it. Must add `cancellation_token: CancellationToken`, loop checks at each turn boundary.

2. **Same-id re-add race**: Old runtime task not fully exited when new one spawns -- two loops on different mpsc receivers. Must `cancel().await + join_handle.await` before insert. Store `HashMap<String, (Arc<AgentHandle>, JoinHandle, CancellationToken)>` not just handle.

3. **In-flight sessions hold Arc<AgentHandle>**: After remove, `registry.get()` returns None but session continues -- OK, but status panel can't see these "zombie sessions". Track zombies or force-cancel sessions before remove.

4. **Which fields can hot-update**:
   - Hot: model, flash_model, max_concurrent (rebuild Semaphore), allowed_commands
   - Warm: system prompt (next turn), skills
   - Cold: workspace, agent_dir, id (= delete + create)

5. **Session state in redb**: Deleting agent leaves session DB intact. Same-id recreate "revives" old conversation. Document this.

6. **Cascading dependencies**: Cron jobs, spawner refs, collaboration configs bound to agent_id. Delete agent must cascade-disable related cron, or refuse with dependency list.

7. **MCP connections**: Each agent maintains MCP client connections. Deleting agent must close MCP transport (stdio/socket), otherwise process-level leak.

8. **Token counters**: session_tokens, last_ctx_tokens etc. Replace = reset or carry? UI cumulative stats will jump.

9. **AgentSpawner staleness**: `gateway/startup.rs:204` constructs `AgentSpawner` once. If it caches an agent list snapshot, hot-added agents are invisible to spawn requests. Either pass `Arc<AgentRegistry>` and re-query each call, or invalidate spawner cache on registry mutation.

10. **Default agent deletion**: `RegistryInner.default_id` may point to the removed agent. See Decision: refuse removal of currently-default agent.

11. **Permission changes vs in-flight tool calls**: `allowed_commands` is hot-mutable. A tool call already dispatched runs to completion under its original permission set; only NEW tool calls see the new policy. Avoids mid-execution kill of long-running commands.

12. **Prompt builder / context manager caches**: `prompt_builder.rs` may cache derived prompts keyed by agent config. On agent update, invalidate any such caches. Audit at implementation time.

## Channel Hot-Reload Pitfalls

1. **No stored JoinHandle**: All channel ingress is `tokio::spawn(async move { ... })` without storing handle. Can't stop. Must refactor all spawns to store JoinHandle + CancellationToken.

2. **Static webhook routes**: Feishu/WeChat/WeCom/DingTalk/Slack use webhooks. Axum Router is compiled at startup. To hot-add: need a single `/webhook/:channel_id` dispatcher with `RwLock<HashMap<channel_id, Arc<dyn WebhookHandler>>>`. Largest single refactor.

3. **Graceful long-connection shutdown**: Telegram long-poll, WhatsApp WS, Slack RTM, Discord Gateway. Direct abort leaves server-side zombie sessions for minutes. Need Close frame handshake + timeout force-kill (two-phase).

4. **Rate limiter state**: Telegram 30msg/sec/chat. Hot-replace channel resets limiter -- may instantly trigger 429 on remote side. Carry over state or cooldown before starting new instance.

5. **Pending queue messages**: Channel deleted but task_queue has pending replies for it. Options: drop (message loss), drain to log, or notify agent to re-route.

6. **Token change = identity change**: Changing Telegram bot token = new bot identity, all chat_id mappings invalid. Same-channel-id token change is effectively delete + add.

7. **WS/UI not notified**: UI control panel fetches channel list once at mount. Hot-reload must push `channel:added/removed/updated` events via event_bus.

8. **Local state files**: Signal/Matrix/WhatsApp have local session DBs. Delete channel: keep files (user may re-enable later). Document.

## Cross-Cutting Concerns

1. **Config write-back**: UI changes in-memory registry, must write back to json5. Atomic write (tmp+rename) + preserve JSON5 comments. Can't use plain serde_json writer.

2. **Validate before apply**: Schema validate + connectivity probe (provider: try /models, channel: try getMe) before swap. Bad config must not break running service.

3. **Unified events**: Every add/remove/update goes through events.rs. New event types must be registered (CLAUDE.md hard rule).

4. **Generation counter**: Each hot-managed resource gets `Arc<AtomicU64>` generation. On replace: increment. Old runtime checks generation mismatch and self-exits. More robust than cancel token alone.

5. **Testing**: Unit tests insufficient. Need integration test `tests/hot_reload.rs`: add -> in-flight -> remove -> re-add full lifecycle.

6. **Non-hot fields**: tantivy index path, listen port, redb path, tokio runtime config. Mark in schema with `restart_required: true` metadata.

7. **Reload audit event**: Every reload emits a `config:reloaded` event_bus message containing: timestamp, source (`file` / `api` / `manual`), applied operations grouped by tier (added/removed/updated arrays for providers/agents/channels), per-op failures. UI subscribes for live config-history view. Also written to `~/.rsclaw/logs/reload.log` for post-mortem. Without this, "my channel disappeared" tickets are unanswerable.

8. **Atomicity model**: Each individual operation (add/remove/update one resource) is atomic. Batch reloads apply changes one-by-one; partial failure is logged but does NOT roll back already-applied changes. Rationale: rolling back a successfully removed agent that already cancelled in-flight sessions would be worse than the partial-apply state. Implication: ordering matters (next item).

9. **Dependency ordering**: Apply order on each reload:
   1. Providers (removes first, then adds — frees names for same-id replace)
   2. Agents (removes, then adds)
   3. Channels (removes, then adds)

   This ensures e.g. a new agent's referenced provider exists before the agent is constructed; same-id replace is naturally handled as remove-then-add. Within a tier, individual operations are independent.

10. **Backpressure during reload**: A reload that briefly removes an agent leaves channel-side messages with no destination. Use the existing per-user `task_queue` as buffer; `agent_registry.get()` miss enqueues rather than drops. Re-add restores delivery. Document max queue depth.

11. **Credential lifetime**: Rust `Arc<Provider>` doesn't zero memory on drop, so revoked API keys may linger in the heap until the last in-flight request finishes (and then GC at allocator's discretion). Acceptable for now; if stricter rotation is required, use `secrecy::Secret<String>` for credentials and explicit zeroize on revoke.

## Implementation Phases

Channel work (phases 5b, 6, 7) is deferred — provider + agent ship first.

| Phase | Scope | Effort | Risk |
|-------|-------|--------|------|
| 1   | Provider same-name update (key/url) — `RwLock<HashMap>` interior + `replace()` API + connectivity probe | 0.5 day | Low |
| 2   | Provider add/remove + default-deletion guard | 1 day | Low |
| 3   | Agent field update (model/system/concurrency) — `ArcSwap` fields, runtime reads via `.load()` | 1 day | Medium |
| 4   | Agent add (new agent) — extract `spawn_one_agent()` from startup | 0.5 day | Low |
| 5a  | Agent remove + `CancellationToken` + JoinHandle map + dependency-reference scan (cron/spawner refs) | 1.5 days | Medium |
| —   | **Provider + Agent shippable here** (~3.5 days total) | | |
| 5b  | Channel `tokio::spawn` -> JoinHandle/CancellationToken refactor (deferred) | 2 days | High |
| 6   | Channel add for long-connection types (Telegram/WhatsApp/Discord/Slack-RTM) | 1 day | Medium |
| 7   | Webhook dispatcher refactor (`/webhook/:channel_id` + handler map) | 2-3 days | High |

Provider + Agent total: **~3.5 days**.
Full scope (incl. channels + webhooks): **~8-9 days**.

## Decisions

- **Provider mutability route**: `RwLock<HashMap<String, Arc<dyn LlmProvider>>>` interior mutability over `ArcSwap<ProviderRegistry>` whole-replace. Avoids any change to `AgentHandle.providers`. Atomic-config-snapshot semantics (everyone-on-same-generation) are not required for our use case.
- **Failover state on provider replace**: **reset** (user changes key as a reset mechanism).
- **Default provider/agent removal**: **REFUSE** removal of currently-default. User must designate a new default first. Silent fallback ("pick the next one") is too surprising.
- **Session state on agent delete**: **keep** (document revival behavior on same-id re-add).
- **Channel local state on delete**: **keep** (may re-enable).
- **Config write-back**: **defer** (not in initial phases, manual edit only).
- **Non-hot fields**: **silently ignore** changes, no restart prompt.
- **In-flight call grace on provider remove**: existing streams run to completion. Documented; revoke-flag is a follow-up if security demands faster rotation.
- **Permission changes (`allowed_commands` etc.)**: apply to NEW tool calls only. In-flight tool calls run to completion under their original permission set.
- **Reload atomicity**: per-operation atomic, batch is best-effort partial-apply (no rollback on later-step failure).
- **Apply order**: providers → agents → channels; within a tier, removes before adds.
- **Audit logging**: every reload emits `config:reloaded` event + `~/.rsclaw/logs/reload.log` line.

## Open Questions

- **Reload API auth**: should `/api/admin/reload` reuse the existing admin scope, or introduce a more restricted `config:write` scope? Current direction: reuse admin scope until a real multi-user model exists.
- **Debounce window**: 2s fixed for editor saves. Make configurable later if needed; not worth the surface area now.
- **Parse failure surfacing**: on bad config, current direction is "keep stale in-memory state, log warning." Should the UI also show a banner ("config invalid since 14:32 — last good state in use")? Probably yes, but Phase 8.
- **Dry-run reload**: a `/reload --dry-run` that reports the diff without applying. Useful for ops; deferred.
- **Per-resource reload**: `/reload provider:openai` style for surgical reloads. Probably unneeded if dependency-ordering is correct, but worth revisiting if batch reload latency becomes an issue.
- **Concurrent reload semantics**: if reload is running and `/reload` is called again, queue the request, drop it, or error? Current direction: drop with warning (prevents pile-up under file-watcher thrash).
