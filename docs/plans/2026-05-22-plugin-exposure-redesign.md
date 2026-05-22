# Plugin exposure redesign — SKILL.md-style catalog

Date: 2026-05-22
Status: approved (brainstorming) — pending implementation plan

## Problem

A plugin can expose hundreds of tools (e.g. the `douyin` plugin has ~230).
Each plugin tool is conceptually a function-calling tool that would normally
live in the LLM request's `tools` field — but injecting hundreds of them blows
up the tool list (cost, KV cache, model confusion). RsClaw therefore proxies
plugin tools through three static meta-tools (`plugin.search_tools`,
`plugin.describe_tool`, `plugin.invoke`); the plugin tools never enter `tools`,
so the cacheable prefix stays byte-stable.

That façade works for the cache but the surrounding UX is muddled:

1. **Catalog is uninformative.** The `## Installed Plugins` block lists a
   plugin name + version + a flat `Example tool names:` CSV + "tool catalog is
   available on demand", pushing the model to `plugin.search_tools` even for
   obvious common tools. It doesn't say what each plugin is *for* or show its
   common tools with purpose.
2. **Capability priority over-deprioritizes built-ins.** The `CAPABILITY
   PRIORITY` block in the shared prefix ranks built-in tools dead last
   ("fallback ONLY"), unconditionally below plugins/skills — so specialized
   built-ins like `computer_use` / `web_browser` get skipped in favour of a
   loosely-matching plugin (or a plugin that isn't even installed), regardless
   of whether a plugin actually fits.
3. **Guidance is unclear** about how the catalog, `search_tools`, and `invoke`
   relate.

## Hard constraint: do not break context (KV cache)

The `tools` field precedes `messages` in the request prefix. Adding, removing,
or reordering entries in `tools` shifts every following token and invalidates
the KV cache from that point. The meta-tool façade exists precisely to keep
`tools` static. **This redesign keeps the façade** — `tools` stays static, so
per-turn cache breakage is zero. (Dynamic tool injection was considered and
rejected for exactly this reason.)

Moving tool definitions into message text is also rejected: it tanks the
model's tool-calling quality (project rule `feedback_tools_in_messages`).

## Non-goals

- No renaming of `plugin.search_tools` / `plugin.describe_tool` /
  `plugin.invoke`. Models are habituated to the word "tool"; renaming to
  "method" risks worse tool-calling. All model-facing text keeps "tool".
- No dynamic injection of plugin tools into the `tools` field.
- No new meta-tool. The three existing ones stay.

## Design

### 1. Terminology (human-facing only)

Model-facing text keeps "tool" everywhere. The conceptual overload (a plugin's
internal tool vs a `tools`-field function tool vs the `plugin.*` meta-tools) is
clarified for human readers in code comments only — no runtime/string change.

### 2. `## Installed Plugins` catalog — written like a SKILL.md

Per-machine content (rendered in `user_system`, NOT the frozen shared-prefix
baseline). For each installed plugin:

```
### <name> — <summary>
<description: what domains/capabilities it covers>
Common tools:
- <tool>: <purpose>
- <tool>: <purpose>
<N> tools total. For others: plugin.search_tools {plugin:"<name>", query}.
```

- **Common tools** is a small curated subset (not all N). Long descriptions
  are truncated to one line. Output is sorted by name for byte-stable
  rendering (KV-cache hygiene within the per-machine layer).
- If a plugin declares no common tools, fall back to the first few tool names
  plus the total count.

### 3. Manifest additions (`plugin.json5`, optional, OpenClaw-compatible)

- `summary` (string, optional): the one-line catalog blurb. Falls back to
  `description` when absent.
- Common-tool marking, either form:
  - per-tool `common: true`, or
  - a plugin-level `commonTools: ["publish", "list_comments", ...]`.
  When neither is present, the catalog shows the first few declared tools + the
  total count.

All new fields are optional; existing manifests parse and render unchanged
(backward compatible).

### 4. Discovery flow (mechanism unchanged, guidance clarified)

The shared-prefix guidance is rewritten to:

1. Consult `## Installed Plugins` below to see which plugins exist and their
   common tools.
2. If a listed tool fits: `plugin.describe_tool {plugin, tool}` for exact
   arguments (when needed), then `plugin.invoke {plugin, tool, arguments}`.
3. If no listed tool fits: `plugin.search_tools {plugin, query}` to discover
   one (scope by `plugin` when the catalog tells you which plugin covers the
   domain), then describe/invoke.

`plugin.search_tools` is unchanged in behavior (intent search over a plugin's
tool catalog, optional `plugin` filter). The guidance simply makes clear it is
for discovering tools beyond the common set, and that it should be scoped by
`plugin` when the plugin is already known from the catalog.

### 5. Capability-priority fix

In the shared-prefix `CAPABILITY PRIORITY` block, remove the unconditional
"built-in tools = fallback ONLY, below plugins/skills" ranking. New framing:
choose the capability that best fits the task — plugins and skills for the
domains their descriptions cover; built-in tools (`computer_use`,
`web_browser`, `shell`, …) are first-class for theirs. Plugins/skills do not
outrank a built-in that is the right tool for the job.

## KV-cache impact

- §2 (catalog) + §3 (manifest) render in the per-machine `user_system` layer →
  do NOT touch the frozen `rsclaw/2026.5.20` base-layer baseline
  (`shared_prefix` + `builtin_tools`).
- §1 (comments only) → no rendered change.
- §4 (guidance) + §5 (priority) live in `shared_prefix` → they change the
  frozen baseline. This requires a one-time fixture re-export
  (`debug dump-prompt-spec --shared-only`) and a corresponding rsclaw-llm
  prefix re-ingest. This is a one-time change, not per-turn churn.
- `tools` stays static (façade preserved) → per-turn cache breakage is zero.

## Affected code

- `src/agent/tools_builder.rs` — `render_plugin_catalog_block` /
  `build_plugins_system` (catalog rendering: summary + common tools).
- `src/plugin/manifest.rs` (+ schema) — `summary`, `common` / `commonTools`.
- `src/agent/prompt_builder.rs` — `build_shared_system_prefix` CAPABILITY
  PRIORITY + plugin-usage guidance.
- `tests/fixtures/baseline-2026.5.20.json` — re-export after the shared-prefix
  changes; `tests/baseline_2026_5_20.rs` stays the gate.

## Testing

- Unit: `render_plugin_catalog_block` renders summary + common tools + total
  count + fallback when no common tools declared.
- Unit: manifest parses `summary` / `common` / `commonTools`; absent fields
  default cleanly.
- Baseline: `cargo test --test baseline_2026_5_20` passes after re-export
  (proves shared-prefix change is the only base-layer drift).
