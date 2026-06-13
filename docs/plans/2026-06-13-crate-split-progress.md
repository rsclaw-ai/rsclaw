# Crate-Split Execution Progress (feat/crate-split)

> Live continuation log for the big-bang split per
> `docs/plans/2026-05-21-crate-split.md`. Mode chosen by owner: **split
> everything first, do NOT compile per-step, then one big `cargo check` +
> fix loop, then e2e compute-use.** Branch: `feat/crate-split` off `dev`.

## Goal (owner-set)
全部拆完 → `cargo check` → 修编译错误 → 编译通过 → e2e 跑 compute-use → 结束.

## The strategy that makes this tractable (re-export shims)

Two directions, both avoid mass ref-site churn:

1. **Type lifted UP into a base crate** (e.g. types): physically move the def to
   `rsclaw-types`, leave `pub use rsclaw_types::X;` at the ORIGINAL site
   (`agent/registry.rs`, `channel/mod.rs`). All existing
   `crate::agent::registry::X` / `crate::channel::X` references keep resolving.
   Downstream crates repoint to `rsclaw_types::X` only when THEY are extracted.

2. **Module moved OUT into its own crate** (e.g. config): move `src/config/` →
   `crates/rsclaw-config/`, then add `pub use rsclaw_config as config;` to the
   root `src/lib.rs`. All root `crate::config::` references resolve unchanged.
   Inside the moved crate, rewrite `crate::config::` → `crate::` (self-refs).

Per-crate commit discipline: every crate is its own commit so any bad
extraction can be reverted or converted to incremental without losing the rest.
Validate each with `cargo metadata --no-deps` (manifest parse only, NOT a build).

## DONE (committed on feat/crate-split)

| crate | source | commit | notes |
|---|---|---|---|
| rsclaw-util | src/util.rs | 921dc3e6 | truncate_str, downscale_image_for_vision. deps: anyhow, image |
| rsclaw-platform | src/sys.rs | 921dc3e6 | MemoryTier + process/runtime. deps: anyhow,libc,sys-info,tokio |
| rsclaw-i18n | src/i18n.rs | 921dc3e6 | t/t_fmt/default_lang (~2.7K lines). no deps |
| rsclaw-events | src/events.rs | 921dc3e6 | AgentEvent + serde DTOs. deps: serde,serde_json,base64 |
| rsclaw-types | (lifted) | b7a3c4e8 | AgentKind{Main,Named,Sub,Task}, ImageAttachment, FileAttachment, OutboundMessage. deps: serde |
| rsclaw-config | src/config/ | 2fb0e213 | clean root, no first-party deps |

Namespace sweep already applied across whole `src/`:
`crate::{util,sys,i18n,events}::` → `rsclaw_{util,platform,i18n,events}::`.
Root `src/lib.rs` re-exports: events, i18n, sys(=platform), util, config.

## LANDMINES found (do not re-trip)

- **TWO `AgentKind` enums, same name, different domains.**
  `agent/registry.rs` = {Main,Named,Sub,Task} (LIFTED to rsclaw-types).
  `cap/runtime.rs` = {Claudecode,Openclaude,Opencode,Codex,Qoder} — driver
  kinds, has inherent impl (from_str/as_str/display_name), stays in runtime knot.
  NEVER blind-sed `AgentKind`. The re-export shim keeps them separate safely.
- Provider wire DTOs (`Message`@provider/mod.rs:155, `ToolDef`@222,
  `ContentPart`@198, `Role`@182, `AgentEndpoint`@248) STAY in rsclaw-provider.
  They churn with protocol work — do NOT put them in rsclaw-types.
- `transcription.rs` does NOT call `crate::provider` (uses raw reqwest/CLI). It
  depends on agent::install_hints, i18n, agent::platform::detect_ffmpeg. So
  media/transcription does not force a channel→provider edge. Keep it in channel.
- gateway records (QueuedTask/Message/File @gateway/task_queue.rs, ExternalJob*
  @gateway/external_jobs.rs, TaskStatus, ExternalJobStatus) are referenced by
  store. To extract store cleanly, lift those records to rsclaw-types FIRST
  (same re-export pattern), which breaks store→gateway.

## REMAINING ORDER (bottom-up)

1. **rsclaw-retry** ← src/channel/retry.rs (SendRetry, send_with_retry). Single
   file. Leave `pub use rsclaw_retry as retry;` where `mod retry;` was in
   channel/mod.rs. Provider keeps its own inline 1-shot retry.
2. **rsclaw-embed** ← src/embed/ (~3 agent refs — check & break or inject).
3. **rsclaw-provider** ← src/provider/ (13K). deps: config, util, types, events.
   Repoint provider's refs to agent attachment types → rsclaw_types::. Keep wire
   DTOs in-crate. root lib.rs: `pub use rsclaw_provider as provider;`.
4. **lift gateway records to rsclaw-types** (QueuedTask/ExternalJob/statuses) via
   re-export shim, THEN **rsclaw-store** ← src/store/.
5. **rsclaw-kb** ← src/kb/ (13K). deps: embed, config, store, +2 agent refs.
6. **rsclaw-channel** ← src/channel/ (20K). deps: types, config, util, retry,
   cap(2). ~119 agent refs were mostly attachment types now in rsclaw-types —
   repoint those to rsclaw_types::. Keep transcription+attachments inside.
7. **leaves** (parallelizable): artifact, mcp, browser, computer, skill, hooks,
   plugin, then **cmd** (17K, top-of-graph), cli. Each: move dir, root re-export
   shim, rewrite self-refs + repoint extracted-crate refs.
8. **runtime knot stays in root**: agent, gateway, server, ws, a2a, acp,
   heartbeat, cap, cron, astock, desktop, migrate. (rsclaw-tools / ToolCtx
   refactor from plan step 6 is DEFERRED — it's a real refactor, not a move.)

## THEN
- One `RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo check` (cold, 8-10min).
  Expect many errors: feature flags per crate, missed cross-crate repoints,
  orphan-rule impls, `pub(crate)` visibility now crossing crate lines (→ `pub`).
- Grind to green (partition errors by crate; parallel fixers OK here).
- Then re-measure (plan step 5): `cargo build --profile release-dev --timings`
  + `lto=false` control — the real payoff verdict.
- e2e: run compute-use end-to-end.

## RESUME
`git checkout feat/crate-split`; read this file + the latest commits; continue
at "rsclaw-retry". The re-export shim pattern is the whole trick — keep using it.
