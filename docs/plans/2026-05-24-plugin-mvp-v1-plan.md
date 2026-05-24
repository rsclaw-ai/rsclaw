# Plugin MVP v1 — Session-Scoped Activation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let users/agents temporarily upgrade a plugin's exposure from "catalog only" (today) to "real ToolDefs in the tools array" via a `/plugin` slash command — so small models on the 4070/5070 fleet can call high-frequency plugin tools directly without the two-step `plugin.search_tools → plugin.invoke` indirection.

**Architecture:**
- Per-session `plugin_overrides` state on `AgentRuntime` (mirror of existing `voice_mode_sessions` HashSet pattern).
- Resolution function: `session override → manifest commonTools → empty`. Default = empty (today's behavior, KV cache untouched).
- When override exists, runtime expands the active set into `<plugin>.<tool>` ToolDefs and appends them to the per-turn tools list at runtime.rs:3090 (after MCP extend, before toolset filter). Dispatch routing for `<plugin>.<tool>` already exists (runtime.rs:7199-7239) — no dispatcher changes needed.
- `/plugin` slash command in preparse mutates the session state and is intercepted host-side (never enters conversation history → doesn't pollute prompt).
- `plugin.search_tools` accepts empty `query` when `plugin` is given, returning a paginated list — gives the model a "browse" mode without adding a new meta-tool.

**Tech Stack:** Rust (existing codebase patterns). Build/test env: `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test`.

**Out of scope for v1 (deferred to v2/v3):**
- Manifest `toolGroups` field — v1 only supports `default | all | <comma-separated names>`.
- Profile/agent-level (persistent) plugin config.
- Pre-warming multiple cache-slot variants on rsclaw-llm.
- Embedding-based scoring.
- Token budget guard (added in v2; v1 cap is a static `MAX_INJECT_TOOLS = 20`).

---

## File Structure

- Modify `src/agent/runtime.rs`:
  - `AgentRuntime` struct: add `plugin_overrides` field.
  - `AgentRuntime::new`: initialize it.
  - New methods: `resolve_plugin_inject`, `expand_plugin_tool_defs`, `set_plugin_override`, `clear_plugin_overrides`.
  - Tool list builder block (line 3090 area): call `expand_plugin_tool_defs` and extend `all`.
  - `tool_plugin_search_tools`: relax empty-query behavior when `plugin` is given; add `offset`.
- Modify `src/gateway/preparse.rs`:
  - Add `/plugin` arm before help/version arms.
  - Add `/plugin` and `/plugin ` to `is_fast_preparse` whitelist.
  - Unit tests.
- Modify `src/agent/tools_builder.rs`:
  - Update `plugin.search_tools` parameters schema (add `offset`).

---

## Task 1: Relax `plugin.search_tools` — empty query becomes "list all" + add offset

**Why:** Gives the LLM and the future `/plugin <name>` command a "browse the plugin's full tool list" capability without introducing a new meta-tool (keeps the cacheable prefix's 4 meta-tools stable).

**Files:**
- Modify: `src/agent/runtime.rs` — `tool_plugin_search_tools` (line 6735).
- Modify: `src/agent/tools_builder.rs` — `plugin.search_tools` schema (line 29-40).
- Test: `src/agent/runtime.rs` (`#[cfg(test)]`) — new tests at the bottom of an existing tests module if one exists; otherwise add a new `#[cfg(test)] mod plugin_search_tests { ... }`.

- [ ] **Step 1: Write the failing tests**

Find the existing tests module in `src/agent/runtime.rs` (search for `#[cfg(test)]`). If none for this area, add at the end of the file:

```rust
#[cfg(test)]
mod plugin_search_tests {
    use super::*;
    use crate::plugin::wasm_runtime::{WasmPlugin, WasmTool};

    fn make_runtime_with_plugin(tools: Vec<(&str, &str)>) -> AgentRuntime {
        // Build a minimal AgentRuntime with one wasm plugin for testing.
        // Use the same pattern as other tests in this file (search for
        // `AgentRuntime::new` test setups). If no helper exists, inline.
        let plugin = WasmPlugin {
            name: "demo".to_owned(),
            version: Some("1.0".to_owned()),
            description: Some("demo plugin".to_owned()),
            summary: Some("demo".to_owned()),
            common_tools: vec!["a".to_owned(), "b".to_owned()],
            tools: tools.into_iter().map(|(n, d)| WasmTool {
                name: n.to_owned(),
                description: d.to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            }).collect(),
            // ... fill other required fields by copying an existing test
        };
        // Reuse whatever test-runtime constructor exists; if none, build via
        // AgentRuntime::new with minimal arguments mirroring existing tests.
        unimplemented!("use existing test scaffold to build runtime with [plugin]")
    }

    #[tokio::test]
    async fn search_empty_query_with_plugin_lists_all() {
        let rt = make_runtime_with_plugin(vec![
            ("a", "tool a"), ("b", "tool b"), ("c", "tool c"),
        ]);
        let result = rt
            .tool_plugin_search_tools(serde_json::json!({"plugin": "demo", "query": ""}))
            .await
            .unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3, "empty query + plugin → list all tools");
        assert!(result.get("error").is_none());
    }

    #[tokio::test]
    async fn search_empty_query_no_plugin_still_errors() {
        let rt = make_runtime_with_plugin(vec![("a", "tool a")]);
        let result = rt
            .tool_plugin_search_tools(serde_json::json!({"query": ""}))
            .await
            .unwrap();
        assert!(result.get("error").is_some(), "no plugin + empty query → error");
    }

    #[tokio::test]
    async fn search_supports_offset_pagination() {
        let rt = make_runtime_with_plugin(vec![
            ("t0", ""), ("t1", ""), ("t2", ""), ("t3", ""), ("t4", ""),
        ]);
        let result = rt
            .tool_plugin_search_tools(serde_json::json!({
                "plugin": "demo", "query": "", "offset": 2, "limit": 2
            }))
            .await
            .unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["tool"], "t2");
        assert_eq!(tools[1]["tool"], "t3");
    }
}
```

> **Note for implementer:** if `AgentRuntime` has no easy test constructor, copy the existing pattern from another `#[cfg(test)] mod` in `runtime.rs` (search for `AgentRuntime::new`). If that's also infeasible, extract `tool_plugin_search_tools`'s body into a pure helper `search_plugin_tools_impl(tools: &[PluginToolInfo], args: &Value) -> Value` and test that instead — same coverage, no runtime needed.

- [ ] **Step 2: Run tests to verify they fail**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo test --lib agent::runtime::plugin_search_tests
```

Expected: FAIL on `search_empty_query_with_plugin_lists_all` (current code returns `{"error": "plugin.search_tools requires non-empty query"}`) and on `search_supports_offset_pagination` (no offset field today).

- [ ] **Step 3: Update `tool_plugin_search_tools`**

Replace the existing body (`src/agent/runtime.rs:6735-6774`) with:

```rust
pub(crate) async fn tool_plugin_search_tools(&self, args: Value) -> Result<Value> {
    let query = args["query"].as_str().unwrap_or("").trim();
    let plugin_filter = args["plugin"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let limit = args["limit"].as_u64().unwrap_or(8).clamp(1, 50) as usize;
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;

    // Empty query with no plugin filter → still an error (would dump every
    // plugin's every tool, defeats the meta-tool's purpose).
    if query.is_empty() && plugin_filter.is_none() {
        return Ok(json!({
            "error": "plugin.search_tools requires either a `query` (search) or a `plugin` (list-all).",
            "hint": "Examples: {plugin:\"douyin\",query:\"publish video\"} or {plugin:\"douyin\"} to list all tools."
        }));
    }

    let all_tools = self.collect_plugin_tools();

    // Browse mode: empty query + plugin given → list-all, alphabetical, paginated.
    if query.is_empty() {
        let plugin = plugin_filter.unwrap();
        let mut tools: Vec<PluginToolInfo> = all_tools
            .into_iter()
            .filter(|t| t.plugin == plugin)
            .collect();
        tools.sort_by(|a, b| a.tool.cmp(&b.tool));
        let total = tools.len();
        let page: Vec<Value> = tools
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|t| Self::plugin_tool_summary(&t))
            .collect();
        return Ok(json!({
            "plugin": plugin,
            "mode": "list",
            "total": total,
            "offset": offset,
            "limit": limit,
            "tools": page,
            "next_offset": if offset + page.len() < total { Some(offset + page.len()) } else { None },
        }));
    }

    // Search mode (existing behavior).
    let mut scored: Vec<(i32, PluginToolInfo)> = all_tools
        .into_iter()
        .filter(|t| plugin_filter.is_none_or(|p| t.plugin == p))
        .map(|t| (Self::score_plugin_tool(query, &t), t))
        .filter(|(score, _)| *score > 0 || plugin_filter.is_some())
        .collect();

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| {
                Self::plugin_runtime_priority(a.1.runtime)
                    .cmp(&Self::plugin_runtime_priority(b.1.runtime))
            })
            .then_with(|| a.1.plugin.cmp(&b.1.plugin))
            .then_with(|| a.1.tool.cmp(&b.1.tool))
    });

    let total = scored.len();
    let page: Vec<Value> = scored
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(score, tool)| {
            let mut summary = Self::plugin_tool_summary(&tool);
            summary["score"] = json!(score);
            summary
        })
        .collect();

    Ok(json!({
        "query": query,
        "plugin": plugin_filter,
        "mode": "search",
        "total": total,
        "offset": offset,
        "limit": limit,
        "tools": page,
        "next_offset": if offset + page.len() < total { Some(offset + page.len()) } else { None },
    }))
}
```

- [ ] **Step 4: Update the meta-tool schema**

`src/agent/tools_builder.rs:29-40` — replace the `plugin.search_tools` ToolDef with:

```rust
ToolDef {
    name: "plugin.search_tools".to_owned(),
    description: "Search or browse installed plugin tool catalogs. With `query` set: ranks tools by intent match. With `query` empty and `plugin` set: lists ALL tools in that plugin alphabetically (use `offset` + `limit` to paginate). Use before plugin.invoke when you need a plugin capability but the exact tool name is not already known.".to_owned(),
    parameters: json!({
        "type": "object",
        "properties": {
            "plugin": {"type": "string", "description": "Optional installed plugin name, e.g. douyin. Omit to search all plugins (requires non-empty query)."},
            "query": {"type": "string", "description": "Short user intent, e.g. 'publish video'. Empty (or omitted) is allowed only when `plugin` is given — then returns the full alphabetical tool list."},
            "limit": {"type": "integer", "description": "Maximum tools to return. Default 8, cap 50."},
            "offset": {"type": "integer", "description": "Pagination offset, default 0. Use with the returned `next_offset` to walk a long plugin."}
        }
    }),
},
```

> Removed the `"required": ["query"]` constraint so empty-query browse works without a workaround.

- [ ] **Step 5: Run tests to verify they pass**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo test --lib agent::runtime::plugin_search_tests
```

Expected: 3 passed.

- [ ] **Step 6: Re-export the baseline fixture (schema changed)**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo run -q -- debug dump-prompt-spec --shared-only -o tests/fixtures/baseline-2026.5.20.json
```

Then verify:

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo test --test baseline_2026_5_20
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/agent/runtime.rs src/agent/tools_builder.rs tests/fixtures/baseline-2026.5.20.json
git commit -m "feat(plugin): search_tools supports browse-all mode + offset pagination"
```

---

## Task 2: Session-level plugin override state

**Why:** Holds the per-conversation activation set the `/plugin` command will mutate. Mirrors the existing `voice_mode_sessions: HashSet<String>` pattern on `AgentRuntime` (runtime.rs:640) for consistency.

**Files:**
- Modify: `src/agent/runtime.rs` — `AgentRuntime` struct, `AgentRuntime::new`, new methods.
- Test: `src/agent/runtime.rs` (`#[cfg(test)]`).

- [ ] **Step 1: Define the override type + struct field**

Near the top of `src/agent/runtime.rs` (above `AgentRuntime`'s struct definition, search for `pub struct AgentRuntime`), add:

```rust
/// Per-session plugin activation override. Set by the `/plugin` slash command;
/// resolved at tool-list build time to decide which plugin tools become
/// directly-callable `<plugin>.<tool>` ToolDefs in the LLM's tools array.
///
/// Default (no entry in `plugin_overrides`) = empty inject set =
/// behave like today (model must use plugin.search_tools + plugin.invoke).
#[derive(Debug, Clone, Default)]
pub(crate) struct PluginOverride {
    /// Tool names to surface as real `<plugin>.<tool>` ToolDefs. Empty = catalog-only.
    pub inject: Vec<String>,
    /// If set, the plugin is hidden entirely (catalog text + tools).
    pub disabled: bool,
}
```

Inside `pub struct AgentRuntime { ... }` (the one ending around line 644), after the `voice_mode_sessions` field, add:

```rust
    /// Per-session plugin activation overrides:
    ///   session_key → { plugin_name → PluginOverride }.
    /// Mutated by the `/plugin` slash command (host-side, never enters
    /// conversation history). Resolved at tool-list build time.
    plugin_overrides: std::collections::HashMap<String, std::collections::HashMap<String, PluginOverride>>,
```

In `AgentRuntime::new` (around line 680-719), after the `voice_mode_sessions: std::collections::HashSet::new(),` line, add:

```rust
            plugin_overrides: std::collections::HashMap::new(),
```

- [ ] **Step 2: Write the failing test**

Add to the same `#[cfg(test)] mod plugin_search_tests` block (or a new sibling test module — keep it close to the new code):

```rust
#[cfg(test)]
mod plugin_override_tests {
    use super::*;

    #[test]
    fn resolve_returns_empty_when_no_override() {
        // Inject set is empty by default (matches today's behavior).
        let overrides: std::collections::HashMap<String, PluginOverride> = Default::default();
        let inject = AgentRuntime::resolve_plugin_inject_pure(&overrides, "douyin");
        assert!(inject.is_empty());
    }

    #[test]
    fn resolve_returns_inject_set_when_override_present() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("douyin".to_owned(), PluginOverride {
            inject: vec!["publish_video".into(), "check_comments".into()],
            disabled: false,
        });
        let inject = AgentRuntime::resolve_plugin_inject_pure(&overrides, "douyin");
        assert_eq!(inject, vec!["publish_video".to_owned(), "check_comments".to_owned()]);
    }

    #[test]
    fn resolve_returns_empty_when_disabled() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("douyin".to_owned(), PluginOverride {
            inject: vec!["publish_video".into()],
            disabled: true,
        });
        let inject = AgentRuntime::resolve_plugin_inject_pure(&overrides, "douyin");
        assert!(inject.is_empty(), "disabled plugin → empty inject");
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo test --lib agent::runtime::plugin_override_tests
```

Expected: FAIL with "no function `resolve_plugin_inject_pure`".

- [ ] **Step 4: Add the resolver + mutators**

In `impl AgentRuntime` block (near the other `tool_plugin_*` methods, e.g. before `tool_plugin_info`), add:

```rust
/// Pure resolver: used by both the tool-list builder and the unit tests.
/// Decides which plugin tools to inject as real ToolDefs for a given
/// (session, plugin) pair. Today only consults the per-session override;
/// v2 will layer profile-level config underneath.
pub(crate) fn resolve_plugin_inject_pure(
    overrides: &std::collections::HashMap<String, PluginOverride>,
    plugin: &str,
) -> Vec<String> {
    match overrides.get(plugin) {
        Some(o) if o.disabled => Vec::new(),
        Some(o) => o.inject.clone(),
        None => Vec::new(),
    }
}

/// Look up the inject set for (session_key, plugin).
pub(crate) fn resolve_plugin_inject(&self, session_key: &str, plugin: &str) -> Vec<String> {
    let Some(per_plugin) = self.plugin_overrides.get(session_key) else {
        return Vec::new();
    };
    Self::resolve_plugin_inject_pure(per_plugin, plugin)
}

/// Set or update a plugin override for a session. Called by the `/plugin`
/// slash command handler. Mutates in place — no DB round-trip (session
/// overrides are intentionally ephemeral; cleared on session reset).
pub(crate) fn set_plugin_override(
    &mut self,
    session_key: &str,
    plugin: &str,
    override_: PluginOverride,
) {
    self.plugin_overrides
        .entry(session_key.to_owned())
        .or_default()
        .insert(plugin.to_owned(), override_);
}

/// Remove all plugin overrides for a session (used by `/plugin reset`).
pub(crate) fn clear_plugin_overrides(&mut self, session_key: &str) {
    self.plugin_overrides.remove(session_key);
}

/// List all plugin overrides for a session (used by `/plugin` status).
pub(crate) fn list_plugin_overrides(
    &self,
    session_key: &str,
) -> std::collections::HashMap<String, PluginOverride> {
    self.plugin_overrides
        .get(session_key)
        .cloned()
        .unwrap_or_default()
}
```

- [ ] **Step 5: Run tests to verify they pass**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo test --lib agent::runtime::plugin_override_tests
```

Expected: 3 passed.

- [ ] **Step 6: Commit**

```bash
git add src/agent/runtime.rs
git commit -m "feat(plugin): session-level plugin override state on AgentRuntime"
```

---

## Task 3: Inject `<plugin>.<tool>` ToolDefs based on session overrides

**Why:** This is the actual payoff — small models on the fleet see real plugin ToolDefs in their tools array (no two-step dance). Dispatch routing for `<plugin>.<tool>` already exists at runtime.rs:7199-7239, so no dispatcher changes needed.

**Files:**
- Modify: `src/agent/runtime.rs` — new helper `expand_plugin_tool_defs`; integration at line 3090 area.
- Test: `src/agent/runtime.rs` (`#[cfg(test)]`).

- [ ] **Step 1: Write the failing test**

Add to the `plugin_override_tests` module:

```rust
#[test]
fn expand_returns_tool_defs_for_inject_set() {
    use crate::plugin::wasm_runtime::{WasmPlugin, WasmTool};
    let plugins = vec![WasmPlugin {
        name: "demo".to_owned(),
        version: Some("1.0".to_owned()),
        description: Some("demo".to_owned()),
        summary: Some("demo".to_owned()),
        common_tools: vec![],
        tools: vec![
            WasmTool {
                name: "publish".to_owned(),
                description: "Publish a thing".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            },
            WasmTool {
                name: "delete".to_owned(),
                description: "Delete a thing".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ],
        // Copy remaining required fields from any existing WasmPlugin test
        // setup in src/plugin/wasm_runtime.rs `#[cfg(test)]`.
    }];

    let mut overrides = std::collections::HashMap::new();
    overrides.insert(
        "demo".to_owned(),
        PluginOverride {
            inject: vec!["publish".into()],
            disabled: false,
        },
    );

    let tool_defs = AgentRuntime::expand_plugin_tool_defs_pure(
        &plugins,
        None, // no js plugins
        &overrides,
        20, // cap
    );
    let names: Vec<&str> = tool_defs.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["demo.publish"]);
    assert_eq!(tool_defs[0].description, "Publish a thing");
}

#[test]
fn expand_caps_total_injected_tools() {
    use crate::plugin::wasm_runtime::{WasmPlugin, WasmTool};
    let plugins = vec![WasmPlugin {
        name: "demo".to_owned(),
        version: None, description: None, summary: None, common_tools: vec![],
        tools: (0..30).map(|i| WasmTool {
            name: format!("t{i}"),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
        }).collect(),
        // (other required fields, see test above)
    }];
    let mut overrides = std::collections::HashMap::new();
    overrides.insert("demo".to_owned(), PluginOverride {
        inject: (0..30).map(|i| format!("t{i}")).collect(),
        disabled: false,
    });
    let defs = AgentRuntime::expand_plugin_tool_defs_pure(&plugins, None, &overrides, 20);
    assert_eq!(defs.len(), 20, "cap enforces max-20 injected tools");
}
```

- [ ] **Step 2: Run tests to verify they fail**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo test --lib agent::runtime::plugin_override_tests::expand
```

Expected: FAIL — `expand_plugin_tool_defs_pure` not defined.

- [ ] **Step 3: Implement the helpers**

Add to `impl AgentRuntime` (near the other resolvers from Task 2):

```rust
/// Build ToolDefs for every plugin tool in any session override's inject set.
/// Names are `<plugin>.<tool>`. Total result is capped at `cap` (v1 hard
/// limit; v2 will swap this for a token-budget guard).
pub(crate) fn expand_plugin_tool_defs_pure(
    wasm_plugins: &[crate::plugin::wasm_runtime::WasmPlugin],
    js_plugins: Option<&crate::plugin::PluginRegistry>,
    overrides: &std::collections::HashMap<String, PluginOverride>,
    cap: usize,
) -> Vec<crate::provider::ToolDef> {
    use crate::provider::ToolDef;
    let mut out: Vec<ToolDef> = Vec::new();

    for wp in wasm_plugins {
        let inject = Self::resolve_plugin_inject_pure(overrides, &wp.name);
        if inject.is_empty() {
            continue;
        }
        for tool_name in &inject {
            if let Some(t) = wp.tools.iter().find(|t| &t.name == tool_name) {
                out.push(ToolDef {
                    name: format!("{}.{}", wp.name, t.name),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                });
                if out.len() >= cap {
                    return out;
                }
            }
        }
    }

    if let Some(reg) = js_plugins {
        for (name, plugin) in reg.js_plugins_iter() {
            let inject = Self::resolve_plugin_inject_pure(overrides, name);
            if inject.is_empty() {
                continue;
            }
            for tool_name in &inject {
                if let Some(t) = plugin.manifest.tools.iter().find(|t| &t.name == tool_name) {
                    out.push(ToolDef {
                        name: format!("{name}.{}", t.name),
                        description: t.description.clone(),
                        parameters: t.input_schema.clone().unwrap_or_else(|| {
                            serde_json::json!({"type": "object", "properties": {}})
                        }),
                    });
                    if out.len() >= cap {
                        return out;
                    }
                }
            }
        }
    }

    out
}

/// Live-runtime convenience: pulls plugins from `self`, looks up session.
pub(crate) fn expand_plugin_tool_defs(&self, session_key: &str) -> Vec<crate::provider::ToolDef> {
    const MAX_INJECT_TOOLS: usize = 20;
    let Some(per_plugin) = self.plugin_overrides.get(session_key) else {
        return Vec::new();
    };
    Self::expand_plugin_tool_defs_pure(
        &self.wasm_plugins,
        self.plugins.as_deref(),
        per_plugin,
        MAX_INJECT_TOOLS,
    )
}
```

- [ ] **Step 4: Wire it into the runtime tool-list builder**

In `src/agent/runtime.rs` around line 3100 (right after `all.extend(mcp.all_tool_defs().await);` and before `// Apply toolset level + custom tools list`), insert:

```rust
            // Plugin overrides (per-session): expand the inject set into
            // real <plugin>.<tool> ToolDefs so small models can call them
            // directly without the plugin.search_tools → plugin.invoke
            // two-step. Dispatch routing for `<plugin>.<tool>` already
            // exists at runtime.rs:7199. Default (no override) = no
            // injection = today's behavior, KV cache stable.
            all.extend(self.expand_plugin_tool_defs(session_key));
```

- [ ] **Step 5: Run tests to verify they pass**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo test --lib agent::runtime::plugin_override_tests
```

Expected: all tests in the module pass.

- [ ] **Step 6: Sanity-build the full binary**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo build --lib
```

Expected: compiles, no warnings beyond pre-existing ones.

- [ ] **Step 7: Commit**

```bash
git add src/agent/runtime.rs
git commit -m "feat(plugin): inject session-overridden plugin tools as real ToolDefs"
```

---

## Task 4: `/plugin` slash command in preparse

**Why:** User-facing UX. The command runs host-side (never enters conversation history → no cache pollution), mutates the session override state, and replies with status text.

**Files:**
- Modify: `src/gateway/preparse.rs` — new `/plugin` arm (insert near `/model` block around line 604); add `/plugin` to `is_fast_preparse` whitelist.
- Test: `src/gateway/preparse.rs` (`#[cfg(test)]`, around line 1100).

> **Wiring note for implementer:** `preparse` currently takes `handle: &AgentHandle` (immutable). The runtime's plugin-override state lives on `AgentRuntime`, which is wrapped inside the live agent loop. There are two viable wiring options:
>
> 1. Move `plugin_overrides` to a shared `Arc<Mutex<HashMap<...>>>` on `AgentHandle` (or `LiveConfig`) so preparse can mutate without needing `&mut AgentRuntime`.
> 2. Send a `PluginOverrideUpdate` message via the existing event bus / inbox, and let the runtime apply it before the next turn.
>
> **Recommended:** option 1 — shorter path, no race with the agent loop (override resolution happens at tool-list build time, which already locks live state). Adjust the `set_plugin_override` / `clear_plugin_overrides` methods from Task 2 to take `&self` and lock the Mutex.
>
> If you choose option 1, redo Task 2's struct definition as `plugin_overrides: Arc<std::sync::Mutex<HashMap<String, HashMap<String, PluginOverride>>>>` and adjust the resolver to `lock().unwrap().get(...)` accordingly. Add a `pub fn plugin_overrides_handle(&self) -> Arc<Mutex<...>>` accessor on `AgentHandle` that returns a clone of the Arc so preparse can mutate it.

- [ ] **Step 1: Rework override storage as `Arc<Mutex<...>>` on `AgentHandle`**

In `src/agent/mod.rs` (or wherever `AgentHandle` is defined — search `pub struct AgentHandle`), add:

```rust
    /// Per-session plugin override state (mutable from preparse).
    pub plugin_overrides: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                std::collections::HashMap<String, crate::agent::runtime::PluginOverride>,
            >,
        >,
    >,
```

(Make sure `PluginOverride` is `pub` in `runtime.rs` — change `pub(crate)` from Task 2 to `pub`.)

Initialize it wherever `AgentHandle` is constructed (search for `AgentHandle {`).

In `AgentRuntime::new`, replace the `plugin_overrides: HashMap::new(),` line with:

```rust
            plugin_overrides: handle.plugin_overrides.clone(),
```

Update the `AgentRuntime` field type to match. Update the resolver/mutator methods from Task 2 to take `&self` (not `&mut self`) and lock the mutex.

- [ ] **Step 2: Write the failing test**

In `src/gateway/preparse.rs`, find the `#[cfg(test)] mod tests` (around line 1100), add:

```rust
#[test]
fn parse_plugin_command_off() {
    let cmd = parse_plugin_command("/plugin douyin off");
    assert_eq!(
        cmd,
        Some(PluginCommand::SetState {
            plugin: "douyin".to_owned(),
            action: PluginAction::Off,
        })
    );
}

#[test]
fn parse_plugin_command_inject_tools() {
    let cmd = parse_plugin_command("/plugin douyin publish,check_comments");
    assert_eq!(
        cmd,
        Some(PluginCommand::SetState {
            plugin: "douyin".to_owned(),
            action: PluginAction::Inject(vec![
                "publish".to_owned(),
                "check_comments".to_owned()
            ]),
        })
    );
}

#[test]
fn parse_plugin_command_reset() {
    assert_eq!(parse_plugin_command("/plugin reset"), Some(PluginCommand::Reset));
}

#[test]
fn parse_plugin_command_status() {
    assert_eq!(parse_plugin_command("/plugin"), Some(PluginCommand::Status));
    assert_eq!(
        parse_plugin_command("/plugin douyin"),
        Some(PluginCommand::Info { plugin: "douyin".to_owned() })
    );
}

#[test]
fn is_fast_preparse_recognizes_plugin() {
    assert!(is_fast_preparse("/plugin"));
    assert!(is_fast_preparse("/plugin douyin"));
    assert!(is_fast_preparse("/plugin douyin off"));
}
```

- [ ] **Step 3: Run tests to verify they fail**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo test --lib gateway::preparse::tests
```

Expected: FAIL — `parse_plugin_command` / `PluginCommand` / `PluginAction` not defined.

- [ ] **Step 4: Add the parser + types**

Near the top of `src/gateway/preparse.rs` (after the `PreparseOrigin` enum, around line 25), add:

```rust
/// Parsed form of a `/plugin ...` command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginCommand {
    /// `/plugin` — list all plugin states for this session.
    Status,
    /// `/plugin <name>` — show one plugin's state.
    Info { plugin: String },
    /// `/plugin <name> off|on|all|<comma-tools>` — set state.
    SetState { plugin: String, action: PluginAction },
    /// `/plugin reset` — clear all session overrides.
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginAction {
    /// Hide the plugin entirely in this session.
    Off,
    /// Inject ALL plugin tools (capped by MAX_INJECT_TOOLS at expand time).
    All,
    /// Clear inject set (= today's catalog-only behavior).
    Default,
    /// Inject this specific tool list.
    Inject(Vec<String>),
}

/// Parse a `/plugin ...` line. Returns None for malformed input (caller falls
/// through to the next preparse arm or prints help).
pub(crate) fn parse_plugin_command(line: &str) -> Option<PluginCommand> {
    let line = line.trim();
    if line == "/plugin" {
        return Some(PluginCommand::Status);
    }
    let rest = line.strip_prefix("/plugin ")?.trim();
    if rest.is_empty() {
        return Some(PluginCommand::Status);
    }
    if rest == "reset" {
        return Some(PluginCommand::Reset);
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let plugin = parts.next()?.trim().to_owned();
    if plugin.is_empty() {
        return None;
    }
    let Some(action_raw) = parts.next().map(str::trim).filter(|s| !s.is_empty()) else {
        return Some(PluginCommand::Info { plugin });
    };
    let action = match action_raw {
        "off" => PluginAction::Off,
        "on" | "default" => PluginAction::Default,
        "all" => PluginAction::All,
        other => {
            let tools: Vec<String> = other
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            if tools.is_empty() {
                return None;
            }
            PluginAction::Inject(tools)
        }
    };
    Some(PluginCommand::SetState { plugin, action })
}
```

- [ ] **Step 5: Hook the command into `try_preparse_locally_with_account`**

In `src/gateway/preparse.rs`, find a clean insertion point (after `/model` block, around line 625, before `/run` shell block) and add:

```rust
    // /plugin — session-scoped plugin activation control.
    if lower == "/plugin" || lower.starts_with("/plugin ") {
        let Some(cmd) = parse_plugin_command(t) else {
            return Some(txt(plugin_help_text()));
        };
        // Resolve session_key from channel + peer_id. There is already a
        // helper for this — search for `derive_session_key` calls in this
        // file; if none, use the same call pattern as the agent inbox.
        let session_key = crate::gateway::session::derive_session_key(&crate::gateway::session::SessionKeyParams {
            agent_id: handle.id.clone(),
            channel: channel.to_owned(),
            peer_id: peer_id.to_owned(),
            kind: crate::gateway::session::MessageKind::DirectMessage { account_id: account.map(str::to_owned) },
            dm_scope: handle.config.session.as_ref().and_then(|s| s.dm_scope.clone()).unwrap_or_default(),
        });
        let overrides = handle.plugin_overrides.clone();
        let reply = match cmd {
            PluginCommand::Reset => {
                let mut g = overrides.lock().unwrap();
                g.remove(&session_key);
                "cleared all plugin overrides for this session".to_owned()
            }
            PluginCommand::Status => {
                let g = overrides.lock().unwrap();
                let map = g.get(&session_key).cloned().unwrap_or_default();
                if map.is_empty() {
                    "no plugin overrides (all plugins at default — catalog only)".to_owned()
                } else {
                    let mut lines = vec!["session plugin overrides:".to_owned()];
                    for (p, o) in &map {
                        if o.disabled {
                            lines.push(format!("  {p}: OFF"));
                        } else if o.inject.is_empty() {
                            lines.push(format!("  {p}: default"));
                        } else {
                            lines.push(format!("  {p}: inject [{}]", o.inject.join(", ")));
                        }
                    }
                    lines.join("\n")
                }
            }
            PluginCommand::Info { plugin } => {
                let g = overrides.lock().unwrap();
                match g.get(&session_key).and_then(|m| m.get(&plugin)) {
                    Some(o) if o.disabled => format!("{plugin}: OFF"),
                    Some(o) if o.inject.is_empty() => format!("{plugin}: default (catalog only)"),
                    Some(o) => format!("{plugin}: inject [{}]", o.inject.join(", ")),
                    None => format!("{plugin}: default (no override)"),
                }
            }
            PluginCommand::SetState { plugin, action } => {
                let new_override = match action {
                    PluginAction::Off => crate::agent::runtime::PluginOverride { inject: vec![], disabled: true },
                    PluginAction::Default => crate::agent::runtime::PluginOverride { inject: vec![], disabled: false },
                    PluginAction::All => {
                        // Resolve "all tools" from the live plugin set. Look up via handle.
                        let all_tools = handle.list_plugin_tool_names(&plugin);
                        if all_tools.is_empty() {
                            return Some(txt(format!("plugin not found: {plugin}")));
                        }
                        crate::agent::runtime::PluginOverride { inject: all_tools, disabled: false }
                    }
                    PluginAction::Inject(tools) => crate::agent::runtime::PluginOverride { inject: tools, disabled: false },
                };
                let summary = if new_override.disabled {
                    format!("{plugin}: OFF")
                } else if new_override.inject.is_empty() {
                    format!("{plugin}: default")
                } else {
                    format!("{plugin}: inject [{}]", new_override.inject.join(", "))
                };
                overrides.lock().unwrap()
                    .entry(session_key.clone())
                    .or_default()
                    .insert(plugin, new_override);
                summary
            }
        };
        return Some(txt(reply));
    }
```

Also add the helper `plugin_help_text()` near the bottom of the file (next to other `*_help_text` functions):

```rust
fn plugin_help_text() -> String {
    "/plugin                       — list session plugin states\n\
     /plugin <name>                — show one plugin's state\n\
     /plugin <name> off            — hide plugin entirely this session\n\
     /plugin <name> on             — back to default (catalog only)\n\
     /plugin <name> all            — inject ALL plugin tools (max 20)\n\
     /plugin <name> t1,t2,t3       — inject specific tools\n\
     /plugin reset                 — clear all session overrides".to_owned()
}
```

And add a helper on `AgentHandle` (in `src/agent/mod.rs` or wherever it lives):

```rust
/// Return all tool names registered by the named plugin (wasm + js).
/// Empty Vec if the plugin is not loaded.
pub fn list_plugin_tool_names(&self, plugin: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(plugins_arc) = self.wasm_plugins.as_ref()
        && let Some(wp) = plugins_arc.iter().find(|p| p.name == plugin)
    {
        out.extend(wp.tools.iter().map(|t| t.name.clone()));
    }
    if let Some(reg) = self.plugin_registry.as_ref()
        && let Some(plugin_handle) = reg.get_js(plugin)
    {
        out.extend(plugin_handle.manifest.tools.iter().map(|t| t.name.clone()));
    }
    out
}
```

> **Implementer:** the field names `wasm_plugins`/`plugin_registry` on `AgentHandle` may differ — adjust to whatever the struct actually exposes. Use the same accessors that `AgentRuntime::new` uses to read the plugin set (search for how `self.wasm_plugins` and `self.plugins` get populated in `AgentRuntime::new`).

- [ ] **Step 6: Add `/plugin` to `is_fast_preparse` whitelist**

In `src/gateway/preparse.rs:688-720`, modify the `is_fast_preparse` function:

```rust
pub(crate) fn is_fast_preparse(text: &str) -> bool {
    let t = text.trim();
    let lower = t.to_lowercase();
    matches!(
        lower.as_str(),
        "/ls" | "/status" | "/version" | "/help" | "/?" | "/health" | "/uptime"
            | "/model" | "/models" | "/cron" | "/clear" | "/new" | "/abort" | "/sessions"
            | "/loop" | "/task" | "/watch" | "/plugin"
    )
    || lower.starts_with("/ls ")
    || lower.starts_with("/cat ")
    || lower.starts_with("/ss")
    || lower.starts_with("/webshot")
    || lower.starts_with("/remember ")
    || lower.starts_with("/recall ")
    || lower.starts_with("/cron ")
    || lower.starts_with("/skill ")
    || lower.starts_with("/plugin ")
    || lower.starts_with("/model ")
    || lower.starts_with("/run ")
    || lower.starts_with("/sh ")
    || lower.starts_with("/exec ")
    || lower.starts_with("/loop ")
    || lower.starts_with("/watch ")
    || lower == "/task -h"
    || lower == "/task --help"
    || lower == "/task help"
    || t.starts_with("! ")
    || t.starts_with("$ ")
}
```

- [ ] **Step 7: Run all preparse tests**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo test --lib gateway::preparse
```

Expected: parser tests + `is_fast_preparse_recognizes_plugin` pass; all existing preparse tests still pass.

- [ ] **Step 8: Build the full binary + lib**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo build
```

Expected: compiles.

- [ ] **Step 9: Add `/plugin` to readonly commands + help text**

In `src/agent/prompt_builder.rs:23-26`, add `/plugin` to `READONLY_COMMANDS` so it's always allowed:

```rust
pub(crate) const READONLY_COMMANDS: &[&str] = &[
    "/help", "/version", "/status", "/health", "/uptime", "/models", "/btw", "/clear", "/compact",
    "/history", "/cron", "/abort", "/loop", "/task", "/plugin",
];
```

Also update `build_help_text_filtered` in the same file to list `/plugin` (search for `/loop` or `/task` lines in that function and add a sibling).

- [ ] **Step 10: Commit**

```bash
git add src/gateway/preparse.rs src/agent/mod.rs src/agent/runtime.rs src/agent/prompt_builder.rs
git commit -m "feat(plugin): /plugin slash command for session-scoped plugin control"
```

---

## Final verification

- [ ] **Step 1: Run the full lib test suite**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo test --lib
```

Expected: PASS. No regressions in adjacent areas (tools_builder, runtime, preparse).

- [ ] **Step 2: Run the baseline gate**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo test --test baseline_2026_5_20
```

Expected: PASS (the baseline was re-exported in Task 1 to absorb the `plugin.search_tools` schema change; nothing in Tasks 2-4 touches the cacheable shared prefix).

- [ ] **Step 3: Clippy**

```
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo clippy --lib
```

Expected: no new warnings.

- [ ] **Step 4: Manual smoke (gateway running, real chat)**

```
# In a chat with a wasm plugin installed (e.g. douyin):
/plugin                          # → "no plugin overrides..."
/plugin douyin                   # → "douyin: default (no override)"
/plugin douyin publish_video     # → "douyin: inject [publish_video]"

# Now ask the LLM: "publish a video"
# Expected: model directly calls douyin.publish_video(...) without first
# calling plugin.search_tools. Watch the tool-call trace.

/plugin reset                    # → "cleared all plugin overrides for this session"
```

---

## Notes for v2 (NOT in this plan)

- Manifest `toolGroups` field → `/plugin <name> <group>` syntax.
- Profile/agent-level persistent plugin config.
- Token-budget guard replaces the static `MAX_INJECT_TOOLS = 20` cap.
- Warning UX when injection would blow the model's context.

## Notes for v3

- rsclaw-llm worker-side pre-warming of multiple inject-set cache slots.
- Embedding-based scoring in `plugin.search_tools` (Qwen3-Embedding-0.6B fleet at 117.50.179.160:5555).
