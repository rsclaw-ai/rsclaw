# Crate Split — Build-Time Refactor Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development`
> (recommended) or `superpowers:executing-plans`. Execute **one extraction at a time**,
> in the order below. After each step the workspace MUST compile (`cargo check`) and the
> full test suite MUST stay green (`cargo test`). Do NOT batch extractions. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Status:** Proposed · **Date:** 2026-05-21 · **Owner decides architecture; Sonnet-class model can execute the mechanical steps.**

---

## Goal

Cut the single monolithic `rsclaw` crate (~160K LOC, 28 modules) into a workspace of
smaller crates so that:

1. Editing one module no longer re-type-checks + re-codegens the whole crate.
2. Crate codegen parallelizes instead of serializing on the final `rsclaw` unit.

This is a **build-iteration-speed** refactor. It is **not** a behavior change — it is
overwhelmingly *moving code + fixing paths*, with one genuinely hard part (breaking the
`agent ↔ channel ↔ gateway` dependency knot).

## Why (measured, not guessed)

`cargo build --timings`, debug profile (`opt-level=0`), forced rebuild of the final
`rsclaw` crate only (deps cached):

| Stage | Time | Note |
|---|---|---|
| **frontend (rmeta)** | **15.1s** | rustc parse + macro expand + type/borrow-check the whole crate |
| **codegen** | **9.3s** | LLVM lowering (even at opt-0, proportional to crate size) |
| **link** | small tail | inside the ~5.8s bin unit; minor |

Conclusions:

- **A faster linker (lld/mold) is the WRONG lever here** — link is the smallest piece.
  Would save ~2-4s. lld was measured/considered and rejected for this codebase.
- **`incremental = true` does not help the 15s frontend** — type-checking is whole-crate;
  editing one function re-checks the entire crate. **Only splitting the crate cuts this.**
- The bottleneck is frontend + codegen of the monolith → splitting is the right (and only)
  fix for the dominant cost.

---

## Dependency analysis

Directed edges = count of `crate::<module>` references. `in` = how many modules depend on
this one; `out` = how many it depends on.

**Hubs (hard, do LAST):**

| module | LOC | out | in | note |
|---|---|---|---|---|
| `agent` | 33K | 18 | 15 | the core; entangled with everything |
| `gateway` | 18K | 18 | 8 | orchestrates agent |
| `channel` | 18K | 6 | 6 | heavy bidirectional with agent |

**Leaves / near-leaves (easy, do FIRST):**

- True leaves (`out=0`, depend on nothing internal): `i18n` (2.5K), `sys`, `events`, `mcp`, `hooks`, `cli`
- Near-leaves: `embed`, `artifact`, `store` (out=2), `provider` (out=2)

### Cycle taxonomy — most cycles are CHEAP (a low-level crate referencing a misfiled type)

| cycle | reality | break by |
|---|---|---|
| `config ↔ agent` (87 / **1**) | the 1 edge is a **doc-comment link** only (`config/schema.rs:1670`) — not a real code dep | trivial; nothing to move |
| `provider ↔ agent` (48 / **1**) | sole edge = `crate::agent::prompt_builder::BUILTIN_TOOL_NAMES` (`provider/rsclaw.rs:2193`) | move the `&[&str]` const into `rsclaw-core` |
| `browser ↔ agent` (14 / **1**) | `crate::agent::platform::detect_chrome` | move the `agent::platform` module into `rsclaw-core` |
| `store ↔ gateway` (14 / 35) | store references `gateway::task_queue::{QueuedTask, TaskStatus}` | move those task_queue **types** into `rsclaw-core` (or `store`) |

### Hard cycles (remain after `rsclaw-core`)

- **`agent ↔ channel` (44 / 109)** — genuine logic coupling; channel heavily calls the agent
  runtime. The biggest knot (18K).
- **`agent ↔ gateway` (11 / 64)** — gateway drives the agent loop.
- These need an **event-bus / trait boundary** (dependency inversion) to separate, or they
  stay together in one `rsclaw-runtime` crate initially.

---

## Design: `rsclaw-core` (the keystone)

A new leaf crate that everything can depend on and that depends on nothing internal. It holds
the shared types currently misfiled inside `agent`/`gateway`/`config`, which is what creates
the cheap cycles. Candidate contents (verify each is dependency-free as you move it):

- config schema types (or split a separate `rsclaw-config`)
- `BUILTIN_TOOL_NAMES` (currently `agent::prompt_builder`)
- `agent::platform` (`detect_chrome`, `detect_powershell_edition`, OS detection)
- `gateway::task_queue` types (`QueuedTask`, `TaskStatus`)
- core message / provider trait types (`Message`, `ToolDef`, `LlmProvider` trait, `Role`, …)
- shared error + event types

Moving these dissolves `config↔agent`, `provider↔agent`, `browser↔agent`, and the type half
of `store↔gateway`.

---

## Extraction order (payoff × feasibility)

Each crate is its own step. Compile + full test suite green before moving on.

- [ ] **0. `rsclaw-core`** — create the workspace + the core crate; move the shared types
      listed above. This is the prerequisite that unlocks the cheap breaks. Expect a wide but
      mechanical diff (`pub(crate)` → `pub` on moved items; `crate::X` → `rsclaw_core::X` at
      call sites). Verify: `cargo check && cargo test`.
- [ ] **1. `rsclaw-provider`** (10K) — single break (`BUILTIN_TOOL_NAMES` now in core). HIGH
      payoff, lowest risk. **Best first real extraction; validates the whole approach.**
- [ ] **2. `rsclaw-store`** (2K) — break the `task_queue` type references (now in core).
      Foundational; many crates depend on it.
- [ ] **3. `rsclaw-config`** (4K) — only the doc-link "cycle"; trivial. (May be folded into
      step 0 if config types went straight to core.)
- [ ] **4. `rsclaw-kb`** (11K, in=3/out=3) — fairly self-contained; good payoff.
- [ ] **5. Leaves** — `i18n`, `embed`, `sys`, `events`, `mcp`, `hooks`, `artifact`, `browser`,
      `computer`, `skill`, `plugin`. Easy; low individual payoff but enables parallel codegen
      and tidies boundaries. Batch in small groups.
- [ ] **6. The knot** — `agent` + `gateway` + `channel` (+ `ws`, `cron`, `a2a`, `server`).
      Either (a) keep as a single `rsclaw-runtime` crate (still a big win: the ~50K+ above is
      already out), or (b) invert `agent↔channel` / `agent↔gateway` via traits/event-bus — a
      deeper, separately-scoped refactor. **Do not attempt (b) cold; design it first.**

---

## Expected payoff

`agent` (33K) dominates frontend+codegen. Even if `agent` stays one crate, pulling
`provider` (10K), `channel` (18K), `kb` (11K), `gateway` (18K) **out** turns the ~24s
monolith unit into "`agent` crate ~10-12s + the rest compiled in parallel", and an edit to
`store`/`provider`/`kb` recompiles only that crate. The `provider` cut alone (step 1) should
produce a visible improvement and de-risk the rest.

---

## Risks & mitigations

- **Runtime risk: LOW** — moving code, not changing logic. The test suite (1100+ lib tests +
  KB e2e) is the safety net; keep it green at every step.
- **Churn / merge-conflict risk: HIGH** — sweeping path edits. Mitigate: one small crate per
  PR; do NOT start while large branches (UI WIP, future feature branches) are mid-flight;
  land each step fast.
- **Cycle-breaking: the real difficulty** — confined to step 0 (move misfiled types) and
  step 6 (the agent/channel/gateway knot). Steps 1-5 are mechanical once core exists.
- **`pub` leakage** — moved items become public crate surface; prefer re-exporting through a
  curated `rsclaw_core::prelude` rather than blanket `pub`.
- **Build-time of the refactor loop itself** — each `cargo check` iteration is minutes on the
  monolith. Acceptable, but it is why this is worth doing.

## Out of scope

- Faster linker (lld/mold): rejected for this codebase (link is not the bottleneck).
- Behavior / API changes.
- The deep `agent↔channel↔gateway` inversion (step 6b) — needs its own design doc.
