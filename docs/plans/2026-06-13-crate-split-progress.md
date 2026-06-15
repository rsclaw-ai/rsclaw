# Crate-Split Execution — COMPLETE ✅

> Big-bang split per `docs/plans/2026-05-21-crate-split.md`. Branch `feat/crate-split`.
> Final state: **22 extracted crates + root bin**, full workspace `cargo check`
> GREEN, debug binary builds, compute-use e2e 3/3.

## RESULT

- **`cargo check` (whole workspace): 0 errors.**
- **`cargo build`: `target/debug/rsclaw` (322 MB) builds.**
- **e2e `cargo test --test computer_e2e -- --include-ignored`: 3 passed / 0 failed**
  (incl. real-display screenshot 2880x1800 @2x).

## 22 EXTRACTED CRATES (crates/*)

Base/leaf (no first-party deps): util, platform, i18n, events, types.
Lower: config, retry, embed, evolution, doc, artifact, mcp, desktop.
Mid: provider, store, memory.
Upper: kb, channel, browser, computer, skill, plugin.

Dependency spine: types/util/platform/i18n/events → config/retry/embed/evolution
→ provider/store/memory/doc → kb/channel/browser/computer → skill/plugin → root.

## ROOT BIN (the runtime knot — stays per plan step 12)

agent, gateway, server, ws, a2a, cap, cmd, cli, cron, heartbeat, migrate,
astock, hooks. These are the bidirectional agent↔gateway↔server core that
cannot be split without trait/event-bus inversion (separate effort).

## KEY TECHNIQUES USED

- **Re-export shims**: type lifted up → `pub use rsclaw_X::Y;` left at old site;
  module moved out → `pub use rsclaw_X as X;` in root lib.rs / agent mod.rs.
  Kept ~thousands of call sites resolving with near-zero ref churn.
- **Generic item extractor** (/tmp/lift.py): brace-matched lift of individual
  fns/structs/enums/consts from large files (runtime.rs, task_queue.rs,
  external_jobs.rs) with leading doc/attr capture.
- **Symbol sinks to break knot edges**: BUILTIN_TOOL_NAMES→types, estimate_tokens
  /expand_tilde/canonicalize_external_path→util, detect_chrome/detect_ffmpeg/
  install_hints→platform, cap::notification→types, gateway records→types,
  evolution/memory/doc→own crates, extract_file_text cluster→channel,
  TurnMetrics→types, OcrClient→kb. ensure_chrome/ensure_ffmpeg (auto-install,
  need cmd::tools) downgraded to detect_* + error.
- **Visibility**: pub(crate)→pub for items now crossing crate lines.
- **hooks reverted to root** (depends on server::AppState + agent::AgentMessage).

## LANDMINES (recorded, all resolved)

- TWO `AgentKind` (agent runtime kinds vs cap driver kinds) — never blind-sed.
- provider wire DTOs stay in provider (churn with protocol).
- memory↔kb "cycle" was a doc-comment, not real — no cycle.
- WIT bindgen path in plugin (src/plugin/wit → src/wit after move).
- gateway cannot be extracted alone (agent↔gateway bidirectional, ~103+108 refs).

## Knot re-analysis (2026-06-14): 5 of 6 'knot' modules ARE extractable
Prior classification was too coarse. Verdicts:
- **heartbeat**: EXTRACTABLE_VIA_TRAIT (effort M)
  - traits: ['Trait: AgentRegistryApi defined in rsclaw-heartbeat { fn get_agent(&self, id: &str) -> Result<AgentHandleProxy>; }. Root (src/agent/registry.rs) provides impl wrapping AgentRegistry -> AgentHandle.']
  - CLEAN EXTRACTION PATH: heartbeat is a self-contained meditation/scheduled-message loop spawned once at gateway startup. Zero reverse edges (gateway only calls it once to spawn, never calls back). All 
- **astock**: EXTRACTABLE_VIA_TRAIT (effort M)
  - traits: ['TRAIT: QueueSubmit — define in new rsclaw-gateway-api crate or extend rsclaw-channel; two methods:']
  - ANALYSIS: astock is a pure A-share market data client + briefing scheduler + SSE notification bridge. It has zero bidirectional dependencies and all reverse edges are one-way (startup init, read-only 
- **cron**: EXTRACTABLE_VIA_SINK (effort L)
  - traits: ['Define Preparser trait in rsclaw-cron-types: trait Preparser { async fn try_preparse(...) -> Option<Reply> }; ROOT (gateway) implements Preparser; CronRunner calls via Arc<dyn Preparser>. This breaks the gateway::preparse circular dependency.']
  - EXTRACTABLE_VIA_SINK verdict: The core cycle is cron↔agent (CronRunner needs AgentRegistry/AgentMessage; agent::tools_cron needs validate_cron_expr + load/save + cron_store). Split strategy: (1) Extra
- **a2a**: EXTRACTABLE_VIA_SINK (effort L)
  - traits: ['Define trait A2aEventEmitter in rsclaw-a2a-types with method emit_agent_event(event: AgentEvent). rsclaw-agent impls for AgentMessage.event_tx. a2a/streaming.rs, a2a/server.rs call through this trait instead of Option::is_some checks.']
  - CRITICAL FINDING: a2a is CLEANLY EXTRACTABLE via 2-3 straightforward sinks, NOT a true cycle. AgentMessage does NOT embed AgentEvent directly — it only has an optional mpsc::Sender<AgentEvent> field s
- **cap**: EXTRACTABLE_VIA_SINK (effort M)
  - traits: []
  - The critical finding: AgentMessage coupling is not a true blocker because inject_followup() is DEAD CODE (marked #[allow(dead_code)], never called in production, commented as "intentionally NOT called
- **ws**: TRUE_CYCLE_LEAVE_ROOT (effort L)
  - traits: ['No trait inversions possible for full extraction: AppState aggregates 30+ runtime subsystems (agents, store, event_bus, computer_permission, shutdown, memory, knowledge, task_store, etc.) and every ws handler method depends on multiple fields. Defining a trait would require 15-20+ accessors and is artificial since AppState IS fundamentally the handler context.']
  - VERDICT: ws is NOT EXTRACTABLE to a standalone crate without creating artificial trait seams. The core blocker is that EVERY ws method handler (50+ functions across 10 modules) has signature `async fn

## FINAL (2026-06-14): 28 crates extracted, knot proven splittable

After the re-analysis, 6 of the "runtime knot" modules WERE extractable —
prior classification was too coarse (it equated "depends on agent/gateway"
with "unsplittable", but the real test is the NATURE of the edge):

| module | how | 
|---|---|
| astock | BriefingSink trait inversion (gateway queue API) |
| cap | dead-code removal (inject_followup) broke the AgentMessage edge |
| heartbeat | HeartbeatHost trait inversion (registry/shutdown/send) |
| cli | clean leaf (zero deps) |
| migrate | repoint to rsclaw_memory/config/store |
| cron | file-internal split: data/persistence -> rsclaw-cron, CronRunner stays root |

GENUINE knot (stays root — AppState-bound RPC / core orchestration, would
cycle without the event-bus inversion of plan step 12):
agent, gateway, server, ws, a2a, cmd, hooks, cron-runner.
- a2a was assessed extractable but streaming.rs/relay.rs are AppState-bound A2A
  RPC handlers (crate::server::{caller_owns,build_agent_card,a2a_rpc_handler_inner})
  — same class as ws. Kept its useful type sinks (ReplyOutcome/StructuredOutcome).

RESULT: 28 crates + root. Full `cargo check` GREEN. `cargo build` ->
target/debug/rsclaw (323MB). computer_e2e 3/3 (real-display screenshot).
