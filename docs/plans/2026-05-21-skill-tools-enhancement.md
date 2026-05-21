# Skill Tools Enhancement — Self-Service Skills for the Agent

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development`
> or `superpowers:executing-plans`. Execute task-by-task; `cargo check` + `cargo test`
> must stay green at each checkbox. This change adds builtin tools → it forces a baseline
> regeneration + an `rsclaw/2026.5.20` server-slot re-register; bundle that with the
> `knowledge_base` slot re-register (see the KB feature) rather than doing it piecemeal.

**Status:** Proposed · **Date:** 2026-05-21 · **Branch:** dev · **Sequence:** after the v2026.5.20 release (with the KB-tool slot re-register).

---

## Goal

Turn skills from "the agent can only ACTIVATE pre-installed skills" into a full
self-service loop: **discover → install → use → clean up**, so the agent can
solve a task it can't handle directly (web_search etc. fail) by finding and
installing a relevant skill (e.g. restaurant/hotel lookup → install `meituan`;
stock screening → `hithink-*`).

## Tool surface (5 tools, `skill_*` family)

Replaces the standalone `use_skill` (kept as an alias for back-compat).

| tool | action | read/write | gate |
|---|---|---|---|
| `skill_list` | list locally-installed skills (live registry) | read | none |
| `skill_search` | search remote registries (`clawhub::search_with_fallback`) | read | none |
| `skill_use` | activate an installed skill; return its SKILL.md | read | none |
| `skill_install` | download + install a skill from a registry | **write** | curated auto / open-registry confirm |
| `skill_remove` | uninstall a local skill (rm dir + lockfile) | **write** | confirm or log |

Naming: singular `skill_*` (each tool acts on one skill; consistent with the
former `use_skill` and with `memory`/`session`/`agent` singular nouns). 5 separate
tools (not a consolidated `skill(action=)`) per product decision — accept the
slightly larger prefix.

## Context economy: progressive disclosure, NOT fastshot distillation

SKILL.md is a **contract** (exact CLI command + flags must survive verbatim).
Do NOT route it through `fastshot` (rsclaw-flash-v1) to summarize — the weakest
model distilling a must-be-exact contract risks garbling the command.

Follow Claude Code's model — progressive disclosure / faithful lazy loading:
1. **Always in context:** skill name + description only (the `## Installed Skills`
   list in `user_system`). Already implemented.
2. **On `skill_use`:** load the full SKILL.md (faithful, no distill). Already done.
3. **Deeper detail:** SKILL.md should reference sub-files (scripts/refs) that the
   agent reads on demand via `read_file` — keep SKILL.md itself lean. (Authoring
   convention + optional: wrap very large SKILL.md via the existing artifact
   pipeline so only a preview + `tool_result_id` enters context, full doc fetched
   with `read_artifact`.)

## Dynamic loading — cache-safe by construction

Skills/plugins live in `user_system`, which the rsclaw provider sends as
`dynamic_prefix.user_system`. The base-layer KV cache is `hash(shared_prefix +
builtin_tools)` and **explicitly excludes `user_system`** (`provider/rsclaw.rs`
comment). So adding/removing a skill never invalidates the base cache — only the
per-session `user_system` layer changes, which is re-sent every turn anyway.

Reload mechanism (reload registry from disk + invalidate `cached_system_prompt`,
which is otherwise only rebuilt on restart):

- [ ] **on `skill_install` success** — so the new skill's description enters
  `user_system` and `skill_use` finds it.
- [ ] **on compact / clear / new** — natural refresh points (already run on
  `&mut self`, already rebuild context).

Cache invalidation is then **automatic and content-addressed**: the rebuilt
prompt is byte-identical when nothing changed (skills + plugins are emitted in
name-sorted, byte-stable order) → cache HIT; it differs only when skills actually
changed → that `user_system` layer re-prefills once (cheap, expected). No manual
"break the cache" logic.

`skill_use` must also **fall back to disk** when a skill isn't in the live
registry yet (freshly installed, before the next reload) — scan the skills dir,
read its SKILL.md — so install→use works the SAME turn.

## Security (write ops)

- `skill_install` from OPEN registries (skills.sh ~91K community skills) =
  supply-chain risk (prompt-injection → install malicious skill). Gate:
  - curated registries (iwencai / hithink, 同花顺-audited) → auto-install OK;
  - open registries → return the install command for the USER to confirm, do not
    auto-install.
- `skill_remove` → at minimum log; prefer confirm.
- `skill_list` / `skill_search` / `skill_use` → read-only, no gate.

## System-prompt guidance

Add to `## Tool Usage Guidelines`: when `web_search` / other tools can't solve a
task, `skill_search` for a relevant skill, `skill_install` it (per the gate), then
`skill_use`. Examples: restaurants/hotels → `meituan`; stock/finance → `hithink-*`.

## Task list

- [ ] Add `skill_search` / `skill_install` / `skill_remove` / `skill_list` +
  rename `use_skill` → `skill_use` (alias `use_skill`). Dispatch handlers in
  `runtime.rs`; reuse `clawhub` for search/install, `cmd/skills.rs` logic for
  install/remove.
- [ ] `skill_use`: registry lookup → disk fallback.
- [ ] Reload-on-(install/compact/clear/new): `self.skills = Arc::new(load_skills(...))`
  + `self.cached_system_prompt = None` (these run on `&mut self`).
- [ ] Install gating by registry trust; remove confirm/log.
- [ ] Add the 5 names to `BUILTIN_TOOL_NAMES`; regenerate baseline; bump count.
- [ ] System-prompt guidance line.
- [ ] Re-register the `rsclaw/2026.5.20` server slot from the new baseline
  (bundle with the `knowledge_base` slot re-register).
- [ ] Verify: `cargo test`; e2e — install a skill, `skill_use` it same turn,
  confirm it appears in `## Installed Skills` after a compact, confirm base KV
  cache stays warm (no churn) across an install.

## Out of scope (separate follow-ups)
- Fixing `IWENCAI_BASE_URL=openapi.iwencai.com` returning 401 (search needs the
  apikey, or use the working `ms.10jqka.com.cn` default) — that's the unrelated
  `skills search hithink` bug.
- Skill authoring guidance for sub-file progressive disclosure.
