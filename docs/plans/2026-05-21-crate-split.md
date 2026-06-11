# Crate Split — Build-Time Refactor Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development`
> (recommended) or `superpowers:executing-plans`. Execute **one extraction at a time**,
> in the order below. After each step the workspace MUST compile (`cargo check`) and the
> full test suite MUST stay green (`cargo test`). Do NOT batch extractions. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Status:** Proposed · **Date:** 2026-05-21 · **Revised:** 2026-06-12 (re-measured LOC/churn; profile mismatch found; added `rsclaw-tools` step and re-measure gate) · **Owner decides architecture; Sonnet-class model can execute the mechanical steps.**

---

## Goal

Cut the single monolithic `rsclaw` crate (~160K LOC at first writing, 28 modules) into a
workspace of smaller crates so that:

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

### ⚠️ 2026-06-12: the measurement above used the WRONG profile

The numbers above are **debug opt-0**. The actual daily workflow is `cargo brd` =
`release-dev` = **opt-level 3 + thin LTO + codegen-units 16**. Under that profile:

- Codegen dominates far harder than the 9.3s debug figure suggests.
- **Thin LTO is whole-program and runs at link time** — crate splitting caches the
  frontend+codegen of untouched crates, but the thin-LTO link tail is paid in full on
  every build regardless of how many crates exist. The "link is small, lld is the wrong
  lever" conclusion above only holds for debug; it is **unverified for release-dev**.

**Required before starting step 1 (and again at every re-measure gate):**

1. `RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo build --profile release-dev --timings`
   on a one-line edit to `agent/runtime.rs` — this is the real baseline.
2. Control experiment: temporarily set `lto = false` in `[profile.release-dev]` and re-time
   the same edit. If this alone recovers a large share of iteration time at acceptable
   runtime cost, it is a zero-day lever that changes the payoff math of the whole plan.

---

## Recommendation (updated after review)

The direction is correct, but the first draft's `rsclaw-core` crate was too broad.
Do **not** create a catch-all core crate containing config, provider traits, platform helpers,
task queue records, and event types. That would become the new compile-time choke point.

Prefer several small, stable interface crates:

| crate | owns | depends on | why |
|---|---|---|---|
| `rsclaw-types` | **stable** cross-crate DTOs only: `ImageAttachment`, `FileAttachment`, `OutboundMessage`, task/external-job records. **NOT provider wire types** (`Message`, `Role`, `ToolDef`, `ContentPart` — these churn with protocol work and live in `rsclaw-provider`, see step 1) | `serde`, `serde_json`, small utilities only | shared language between runtime crates |
| `rsclaw-config` | JSON5 schema, loader, env/secrets resolution, proxy client construction | `rsclaw-types` where needed | many crates need config, but config should not know runtime modules |
| `rsclaw-platform` | OS/tool detection: Chrome, ffmpeg, PowerShell, base tool paths | `rsclaw-config` or a small path provider | breaks current `agent::platform` leakage |
| `rsclaw-retry` | `RetryConfig`, backoff helpers, transport retry primitives | none or small deps | avoids making channels depend on `provider` |
| `rsclaw-media` | attachment classification, upload naming, office/PDF text extraction, audio transcription facade | `rsclaw-types`, `rsclaw-platform`, `rsclaw-config` | removes false `agent ↔ channel` coupling |

Only after those seams exist should the large functional crates move out. Keep the root
`rsclaw` crate as a facade during migration so integration tests and existing module paths can
be updated incrementally.

---

## Dependency analysis

Directed edges = count of `crate::<module>` references. `in` = how many modules depend on
this one; `out` = how many it depends on.

**Hubs (hard, do LAST):**

| module | LOC (05-21) | LOC (06-12) | out | in | note |
|---|---|---|---|---|---|
| `agent` | 33K | **45K** | 18 | 15 | the core; entangled with everything |
| `gateway` | 18K | **22K** | 18 | 8 | orchestrates agent |
| `channel` | 18K | **20K** | 6 | 6 | heavy bidirectional with agent |

The hubs grew ~25% in three weeks. The growth rate itself is an argument for starting the
cheap steps now — every week of delay raises the import-churn cost of each extraction.

`cmd` (**17K LOC** — bigger than `provider`) was missing from the first draft entirely. It
sits at the top of the graph (depends on everything, nothing depends on it), so extracting
it is cheap and stops `cmd` edits from re-type-checking the monolith. Added to step 11.

### The real hotspot: `agent/runtime.rs` (added 2026-06-12)

90-day file-touch counts inside `src/agent`:

| file | touches | LOC |
|---|---:|---:|
| `agent/runtime.rs` | **440** | 12.4K |
| `agent/tools_builder.rs` | 108 | 2.5K |
| `agent/prompt_builder.rs` | 64 | 1.2K |
| `agent/registry.rs` | 60 | 1.1K |
| `agent/tools_web.rs` | 55 | 2.8K |

`runtime.rs` alone takes 4× the churn of the next file — it is the single hottest edit
path in the repo, and the first draft deferred everything that would help it to step 10
("only if still the bottleneck"). A plan that ships steps 1–8 and stops leaves the most
common edit recompiling a ~80K-LOC runtime crate + bin + LTO link: roughly a 2× win for
the daily loop, not an order of magnitude.

Structural finding: all `tools_*.rs` files (~15K LOC total) are `impl AgentRuntime`
method blocks (e.g. `pub(crate) async fn tool_web_search(&self, ...)`). This coupling is
*the* structural reason `agent` cannot shrink. Breaking it does not require the full
event-bus inversion — a narrow `ToolCtx` (providers/config/store handles) turns tools
into free functions that can sink below the runtime. See step 6.

**Leaves / near-leaves (easy, but not all first):**

- True leaves (`out=0`, depend on nothing internal): `i18n` (2.5K), `sys`, `events`, `mcp`, `hooks`, `cli`
- Near-leaves: `embed`, `artifact`, `store` (out=2), `provider` (out=2)

### Cycle taxonomy — most cycles are CHEAP (a low-level crate referencing a misfiled type)

| cycle | reality | break by |
|---|---|---|
| `config ↔ agent` (87 / **1**) | the 1 edge is a **doc-comment link** only (`config/schema.rs:1670`) — not a real code dep | trivial; nothing to move |
| `provider ↔ agent` (48 / **1**) | sole edge = `crate::agent::prompt_builder::BUILTIN_TOOL_NAMES` (`provider/rsclaw.rs:2193`) | move the `&[&str]` const into `rsclaw-types` or a provider-local constants module |
| `browser ↔ agent` (14 / **1**) | `crate::agent::platform::detect_chrome` | move `agent::platform` into `rsclaw-platform` |
| `store ↔ gateway` (14 / 35) | store references `gateway::task_queue::{QueuedTask, TaskStatus}` **and** `gateway::external_jobs::{ExternalJob, ExternalJobStatus}` | move both task queue and external job persistent records into `rsclaw-types`, or define store-owned persistence records |

### Hard cycles (remain after shared DTO/helper cleanup)

- **`agent ↔ channel` (44 / 109)** — partly real, partly false coupling. Channel currently
  depends on `agent::registry::{ImageAttachment, FileAttachment}`, `agent::platform`,
  `agent::doc`, and `agent::install_hints`. Agent depends on channel for `OutboundMessage`,
  attachment classification, upload paths, office extraction, and transcription. Move DTOs and
  media helpers first; only the remaining runtime callback/notification boundary should need
  design.
- **`agent ↔ gateway` (11 / 64)** — gateway drives the agent loop.
- These need an **event-bus / trait boundary** (dependency inversion) to separate, or they
  stay together in one `rsclaw-runtime` crate initially.

---

## Channel crate decision

`channel` is worth extracting, but **not as the first crate and not by moving `src/channel`
unchanged**.

Why it is a good eventual crate:

- Size is meaningful: `src/channel` is ~18K LOC.
- The product boundary is real: channels adapt external messaging networks into RsClaw's
  inbound/outbound message model.
- A channel edit should not force agent/provider/kb/server codegen.

Why it should wait:

- Current channel files call agent-owned DTOs (`ImageAttachment`, `FileAttachment`).
- Current channel files call agent-owned platform/doc/install helpers.
- Agent code calls channel-owned media helpers and transcription.
- Gateway/server/cmd code directly names concrete channel types for startup, webhook handling,
  and QR/onboarding flows.
- `DmPolicyEnforcer` optionally persists through `RedbStore`, which would make a standalone
  channel crate depend on store unless a small persistence trait is introduced.

Target shape before extraction:

- `rsclaw-channel` depends on `rsclaw-types`, `rsclaw-config`, `rsclaw-retry`,
  `rsclaw-platform`, and `rsclaw-media`.
- `rsclaw-channel` does **not** depend on `rsclaw-agent`, `rsclaw-gateway`,
  `rsclaw-provider`, or concrete `RedbStore`.
- Pairing persistence is behind a small trait, implemented/adapted by the gateway/store side.
- The first extraction should be one `rsclaw-channel` crate. Do not split per-channel crates
  (`rsclaw-channel-telegram`, etc.) until the aggregate crate is working; per-channel crates
  add workspace overhead before proving payoff.

---

## Extraction order (payoff × feasibility)

Each step should compile before moving on. For extraction PRs, run:

```bash
RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo check
RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo test
```

- [ ] **0. Workspace scaffold + facade policy** — convert root package to a workspace,
      add `crates/`, keep the root `rsclaw` package/bin as the compatibility facade.
      Do not move large modules yet. Verify: `cargo check && cargo test`.
- [ ] **1. `rsclaw-types`** — move stable DTOs only: `ImageAttachment`, `FileAttachment`,
      `OutboundMessage`, task queue persistent records, external job persistent records,
      and small enums that are serialized across modules. Keep logic out.
      **Revised 2026-06-12 — wire DTOs are NOT stable here:** provider wire types
      (`Message`, `ToolDef`, `ContentPart`, …) churn with every protocol iteration
      (incremental protocol, constrained tool-calls, v1 arg fixes). If they land in a
      frozen `types`, every protocol edit becomes a full-workspace rebuild — the monolith
      bottleneck moved down one level. Keep provider wire DTOs in `rsclaw-provider`;
      `types` holds only cross-crate types that have actually been stable for months.
- [ ] **2. `rsclaw-config`** — move schema, loader, env/secrets resolution, and proxy client
      construction. This should depend on `rsclaw-types` where needed, not on runtime crates.
- [ ] **3. `rsclaw-platform` + `rsclaw-retry`** — move OS/tool detection and generic retry
      helpers. This breaks the easy provider/channel/browser/platform leakage without growing
      `types`.
- [ ] **4. `rsclaw-provider`** (13K) — after `BUILTIN_TOOL_NAMES` is out of agent, extract
      provider implementations, wire DTOs, registry, defaults, and failover. This is the
      best first large payoff extraction.
- [ ] **5. RE-MEASURE GATE** — `cargo build --profile release-dev --timings` on a one-line
      `runtime.rs` edit, plus the `lto = false` control experiment (see the profile warning
      above). The result decides priority between step 6 (tools, hits the hot path) and
      steps 8–10 (kb/media/channel, isolates cold churn). Do not skip this and proceed on
      the 05-21 debug-profile numbers.
- [ ] **6. `rsclaw-tools` via `ToolCtx`** (~15K) — **added 2026-06-12; the only step that
      directly targets the 440-touch/90-day hot path.** Introduce a narrow `ToolCtx`
      exposing provider/config/store handles; convert `tools_*.rs` from `impl AgentRuntime`
      blocks into free functions over the ctx; sink them into a `rsclaw-tools` crate that
      depends on types/config/provider and **not** on the runtime. Effect is two-way:
      editing `runtime.rs` recompiles only the runtime crate; editing a tool recompiles
      tools + runtime (same as today). This is a real refactor, not a file move — but it is
      far smaller than the step-12 event-bus inversion and attacks the same knot from
      inside. `tools_builder.rs` (ToolDef assembly, 108 touches) likely stays runtime-side
      initially; decide per-function during the move.
- [ ] **7. `rsclaw-store`** (2K+) — extract only after both task queue and external job
      records no longer live in gateway. Store should not depend on gateway.
- [ ] **8. `rsclaw-kb`** (13K) — extract after config is available. Current KB code has only
      small config edges (`config::load`, `EmbedConfig`); either keep those through
      `rsclaw-config` or inject the config explicitly before moving.
- [ ] **9. `rsclaw-media`** — move attachment classification, upload naming/path helpers,
      office/PDF extraction, and transcription facade. This prepares channel extraction and
      removes agent's dependency on channel helpers.
      **⚠️ Verify before committing media's dep set:** `transcrib*` is referenced across
      several channels AND in `agent/runtime.rs` — and runtime.rs is the only one of those
      that imports `crate::provider`. If transcription routes through a provider/STT call,
      then `media → provider`, and since `channel → media`, channel transitively depends on
      provider — violating the channel boundary (see "Channel crate decision" target shape).
      Resolve the actual transcription call path first; if it needs provider, keep
      transcription behind a **trait injected by the runtime**, not a concrete facade owned
      by `media`.
- [ ] **10. `rsclaw-channel`** (20K) — extract the aggregate channel crate once it depends
      only on types/config/platform/retry/media and a small pairing persistence trait. Keep
      gateway startup/webhook wiring in gateway/server; channel owns protocol clients and
      send/run implementations.
- [ ] **11. Small leaves + `rsclaw-cmd`** — extract `i18n`, `events`, `hooks`, `embed`,
      `artifact`, `mcp`, `plugin`, `skill`, `browser`, `computer` where the remaining
      dependency graph is clean. Batch only genuinely independent leaves. Also extract
      `cmd` (17K) here: it is top-of-graph (nothing depends on it), so the move is cheap
      and stops CLI edits from re-type-checking the runtime.
- [ ] **12. Runtime knot** — keep `agent` + `gateway` + `server` + `ws` + `a2a` + `acp`
      together as `rsclaw-runtime` initially. Re-measure. Only design event-bus/trait
      inversion if this remaining crate is still the dominant edit-time bottleneck.

---

## Expected payoff

`agent` (45K) dominates frontend+codegen, but a large part of the current cost is that every
small edit shares the same final crate. Pulling `provider` (13K), `kb` (13K), `channel` (20K),
`cmd` (17K), media helpers, config, and store out should make edits to those areas compile only
the touched crate plus dependents. The remaining runtime crate may still be substantial, but it
will no longer absorb unrelated provider/channel/kb churn.

Re-measure at step 5 (mandatory gate) and again after steps 6, 8, 10, and 12 with
`cargo build --profile release-dev --timings` — **the daily-driver profile, not debug**. Do not
continue splitting purely for tidiness if timing data says the bottleneck moved elsewhere.

**Honest payoff caveat — without step 6, the split speeds up the modules you edit LEAST.**
90-day edit churn (file-touches per top-level module): **`agent` 1044, `gateway` 491**,
`provider` 225, `channel` 223, `kb` 218, `config` 99, `server`/`ws` ~80 each. Within `agent`,
`runtime.rs` alone is 440 touches. The first draft deferred everything that helps that hot path
to the final step ("only if still the bottleneck"); the 2026-06-12 revision adds step 6
(`rsclaw-tools` via `ToolCtx`) precisely to carve ~15K LOC of tools out of the hot crate so a
`runtime.rs` edit recompiles a markedly smaller unit. Even so: until step 6 lands, do not
expect day-to-day agent edits to get faster — steps 1–4 isolate provider/kb/channel churn,
nothing more. And the thin-LTO link tail (see profile warning) is paid every build regardless
of crate count; only the step-5 measurement can say how big that floor is.

---

## Effort estimate

These are wall-clock estimates for a focused AI-assisted session with current test behavior.
They include compile/test loops, not review/merge waiting.

| phase | expected time | notes |
|---|---:|---|
| Workspace scaffold + `rsclaw-types` | 0.5-1.5 days | wide import churn, low logic risk |
| `rsclaw-config` + `rsclaw-platform` + `rsclaw-retry` | 0.5-1 day | mostly mechanical, but config is referenced everywhere |
| `rsclaw-provider` | 0.5-1 day | high payoff, moderate dependency cleanup |
| Re-measure gate (step 5) | 0.5 day | release-dev `--timings` + `lto = false` control; decides 6 vs 8–10 priority |
| `rsclaw-tools` via `ToolCtx` (step 6) | 1-2 days | real refactor (impl blocks → ctx functions), not a file move; the only step on the hot path |
| `rsclaw-store` | 0.5 day | only if task + external-job records are already moved |
| `rsclaw-kb` | 0.5-1 day | self-contained, but has many files and e2e coverage |
| `rsclaw-media` prep | 0.5-1 day | important for channel; avoid mixing with channel extraction |
| `rsclaw-channel` | 1-2 days | many concrete integrations and gateway/server/cmd touch points |
| Remaining leaves + `rsclaw-cmd` | 0.5-1.5 days | depends on how clean the graph is after earlier steps |
| Runtime knot re-measure/design | 0.5 day for measurement, more only if inversion is needed | do not start deep inversion without a separate plan |

Practical total: **5-10 focused working days** for the useful split through channel, assuming
tests are healthy and no large feature branches are competing. A conservative calendar estimate
is **1-2 weeks** because this kind of refactor benefits from small PRs and review checkpoints.

---

## Parallelism policy

This refactor can use parallel agents, but only after the dependency foundation is in place.

Safe to parallelize:

- Audit-only work: dependency edge maps, LOC/timing measurement, per-module extraction notes.
- Independent small leaves after `types/config/platform/retry` are stable.
- Test fixes scoped to disjoint crates after an extraction compiles.

Do **not** parallelize:

- Workspace scaffold and `Cargo.toml` topology.
- `rsclaw-types` moves, because every downstream path depends on them.
- `rsclaw-config`, because it is referenced across most modules.
- `rsclaw-channel` extraction with `rsclaw-media` extraction; these touch the same call sites.
- Multiple large extraction PRs that rewrite imports across `agent`, `gateway`, and `server`.
- The `ToolCtx` refactor (step 6) with ANY other agent-touching work — it rewrites every
  `tools_*.rs` and `runtime.rs` dispatch site.

Recommended execution model:

1. One lead agent owns the dependency graph and workspace topology.
2. One worker at a time performs a large extraction.
3. Optional side workers prepare read-only audits or isolated leaf crates.
4. After each extraction, run `cargo check`, targeted tests for the moved module, then full
   `cargo test`.

---

## Risks & mitigations

- **Runtime risk: LOW** — moving code, not changing logic. The test suite (1100+ lib tests +
  KB e2e) is the safety net; keep it green at every step.
- **Churn / merge-conflict risk: HIGH** — sweeping path edits. Mitigate: one small crate per
  PR; do NOT start while large branches (UI WIP, future feature branches) are mid-flight;
  land each step fast.
  **Current-state gate (re-checked 2026-06-12, still holds):** there is uncommitted WIP
  across exactly the hot files (`agent/runtime.rs`, `channel/feishu.rs`, `gateway/preparse.rs`,
  `config/schema.rs`, …). Steps **9–10 (media + channel) touch those call sites — do NOT
  start them until in-flight channel work lands.** Steps 0–4 (scaffold, types, config,
  platform/retry, provider) are largely orthogonal and may proceed in the meantime. Step 6
  (tools/`ToolCtx`) rewrites `tools_*.rs` wholesale — it needs a clean `src/agent` working
  tree before starting.
- **`rsclaw-types` becomes the new choke point if it churns** — every crate depends on it, so
  any edit to `types` triggers a full-workspace rebuild. After step 1, `types` MUST be frozen /
  append-only. If a DTO needs behavior or non-trivial deps, it does not belong in `types`.
  A churning `types` just moves the monolith bottleneck down one level. **This is not
  hypothetical:** provider wire types churn with every protocol iteration, which is why
  step 1 now keeps them in `rsclaw-provider` instead.
- **build.rs / version stamping** — the `RSCLAW_BUILD_VERSION` / `RSCLAW_BUILD_DATE` plumbing
  (see the check commands) must stay in the root facade **bin** crate; do not scatter it into
  library crates. Re-plumbing **cargo feature flags** across the workspace (zip/bzip2/etc.) is
  usually minor but is real work — account for it when a moved module is feature-gated.
- **Cycle-breaking: the real difficulty** — the false cycles are mostly misfiled DTOs/helpers;
  the real runtime knot should be deferred until after the graph is cleaned and re-measured.
- **`pub` leakage** — moved items become public crate surface; prefer re-exporting through a
  curated facade rather than blanket `pub`.
- **Build-time of the refactor loop itself** — each `cargo check` iteration is minutes on the
  monolith. Acceptable, but it is why this is worth doing.
- **Over-splitting** — too many tiny crates increase workspace complexity and can worsen clean
  builds. Stop when timings show the remaining bottleneck no longer justifies more boundary work.

## Out of scope

- Faster linker (lld/mold): rejected **for the debug profile** (link is not the bottleneck
  there). Under release-dev the link/LTO stage is unmeasured — the step-5 gate covers it via
  the `lto = false` experiment; revisit only if that measurement says otherwise.
- Behavior / API changes.
- Per-channel crates (`rsclaw-channel-telegram`, etc.) until aggregate `rsclaw-channel` has
  proven useful.
- The deep `agent↔gateway` / runtime event-bus inversion — needs its own design doc after the
  useful low-risk crates are already split.
