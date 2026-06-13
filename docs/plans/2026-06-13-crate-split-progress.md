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
