# Plugin Exposure Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `## Installed Plugins` prompt catalog informative (SKILL.md-style: summary + curated common tools), keep the static meta-tool façade (zero KV-cache breakage), clarify plugin-usage guidance, and stop ranking specialized built-in tools unconditionally below plugins.

**Architecture:** Plugins stay behind the three static meta-tools (`plugin.search_tools` / `plugin.describe_tool` / `plugin.invoke`) — the `tools` array never changes, so the cacheable prefix is untouched. We enrich only (a) the per-machine catalog text and (b) the shared-prefix guidance/priority text. Plugin manifests gain optional `summary` + `commonTools`; the catalog renderer uses them to show each plugin's purpose and a few common tools, pointing at `plugin.search_tools` for the rest.

**Tech Stack:** Rust, serde/json5 (manifests), existing prompt builders. Build/test env: `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test`.

---

## File Structure

- `src/plugin/manifest.rs` — `PluginManifest` gains `summary: Option<String>` and `common_tools: Vec<String>` (`commonTools`). Parsing test.
- `src/plugin/wasm_runtime.rs` — `WasmPlugin` gains `summary` + `common_tools`, populated from the manifest in `load_wasm_plugin`.
- `src/agent/tools_builder.rs` — replace `plugin_tool_examples` with `plugin_tool_list` (renders `- name: purpose` lines, common-first); rewrite `render_plugin_catalog_block`; update `build_plugins_system` (wasm + js call sites). Unit tests.
- `src/agent/prompt_builder.rs` — rewrite the plugin-usage guidance + `CAPABILITY PRIORITY` block in `build_shared_system_prefix`.
- `tests/fixtures/baseline-2026.5.20.json` — re-export after the shared-prefix change; `tests/baseline_2026_5_20.rs` is the gate.

Note: model-facing wording keeps "tool" (no renames). Terminology clarification (spec §1) is comments only — folded into the tasks below, not a separate task.

---

### Task 1: Manifest fields — `summary` + `commonTools`

**Files:**
- Modify: `src/plugin/manifest.rs` (`PluginManifest` struct)
- Test: `src/plugin/manifest.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Add to the manifest test module:

```rust
#[test]
fn manifest_parses_summary_and_common_tools() {
    let json5 = r#"{
        name: "demo",
        version: "1.0.0",
        description: "d",
        summary: "Does demo things",
        commonTools: ["publish", "list"],
        tools: [{ name: "publish", description: "p" }],
    }"#;
    let m: PluginManifest = json5::from_str(json5).unwrap();
    assert_eq!(m.summary.as_deref(), Some("Does demo things"));
    assert_eq!(m.common_tools, vec!["publish".to_string(), "list".to_string()]);

    // Backward compat: absent fields default cleanly.
    let bare = r#"{ name: "x", tools: [] }"#;
    let m2: PluginManifest = json5::from_str(bare).unwrap();
    assert!(m2.summary.is_none());
    assert!(m2.common_tools.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo test --lib plugin::manifest::tests::manifest_parses_summary_and_common_tools`
Expected: FAIL — no field `summary` on `PluginManifest`.

- [ ] **Step 3: Add the fields**

In `PluginManifest`, after the `description` field:

```rust
    /// One-line catalog blurb shown in the `## Installed Plugins` prompt
    /// section. Falls back to `description` when absent.
    #[serde(default)]
    pub summary: Option<String>,
    /// Tool names to surface as "common" in the catalog (the rest are found
    /// via plugin.search_tools). Empty → renderer shows the first few + count.
    #[serde(default, rename = "commonTools")]
    pub common_tools: Vec<String>,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo test --lib plugin::manifest::tests::manifest_parses_summary_and_common_tools`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/plugin/manifest.rs
git commit -m "feat(plugin): manifest summary + commonTools fields"
```

---

### Task 2: Carry `summary` + `common_tools` onto `WasmPlugin`

**Files:**
- Modify: `src/plugin/wasm_runtime.rs` (`WasmPlugin` struct + `load_wasm_plugin`)

WASM plugins are exposed to the catalog as `WasmPlugin` (not the raw manifest), so the two fields must be copied across at load time. JS plugins already expose `plugin.manifest` to the catalog and need no change here.

- [ ] **Step 1: Add fields to `WasmPlugin`**

After the `description` field in `pub struct WasmPlugin`:

```rust
    /// Catalog summary (from manifest `summary`, else None → falls back to
    /// `description` at render time).
    pub summary: Option<String>,
    /// Tool names the manifest marks as common (from `commonTools`).
    pub common_tools: Vec<String>,
```

- [ ] **Step 2: Populate them in `load_wasm_plugin`**

Find where the `WasmPlugin { ... }` literal is constructed in `load_wasm_plugin` and add (the function already has `manifest: &PluginManifest` in scope):

```rust
        summary: manifest.summary.clone(),
        common_tools: manifest.common_tools.clone(),
```

- [ ] **Step 3: Build to verify it compiles**

Run: `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo build --lib`
Expected: compiles (no missing-field error on `WasmPlugin`).

- [ ] **Step 4: Commit**

```bash
git add src/plugin/wasm_runtime.rs
git commit -m "feat(plugin): WasmPlugin carries summary + common_tools from manifest"
```

---

### Task 3: SKILL.md-style catalog renderer

**Files:**
- Modify: `src/agent/tools_builder.rs` (replace `plugin_tool_examples` + `render_plugin_catalog_block`)
- Test: `src/agent/tools_builder.rs` (`#[cfg(test)]`)

`plugin_tool_list` renders `- name: purpose` lines, common tools first then the rest up to a cap, with an overflow pointer to `plugin.search_tools`. Sorted within each group for byte-stable output.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn catalog_lists_common_tools_first_with_overflow() {
    // (name, description) pairs; "publish"/"list" are common.
    let tools = vec![
        ("zeta", "z"), ("publish", "Publish a video"),
        ("list", "List items"), ("alpha", "a"),
    ];
    let common = vec!["publish".to_string(), "list".to_string()];
    let out = plugin_tool_list(&tools, &common, 3);
    // Common first, in commonTools order; then others by name; capped at 3.
    assert_eq!(
        out,
        "- publish: Publish a video\n- list: List items\n- alpha: a\n- …1 more tool — plugin.search_tools to find them"
    );

    // No common declared → first N by name + overflow.
    let none: Vec<String> = vec![];
    let out2 = plugin_tool_list(&tools, &none, 2);
    assert_eq!(out2, "- alpha: a\n- list: List items\n- …2 more tools — plugin.search_tools to find them");

    // Empty.
    assert_eq!(plugin_tool_list(&[], &none, 5), "  (no declared tools)");
}

#[test]
fn catalog_block_is_skill_md_shaped() {
    let tools = vec![("publish", "Publish a video")];
    let block = render_plugin_catalog_block("douyin", "0.1.0", "Douyin ops", &tools, &[], 5);
    assert!(block.contains("### douyin — Douyin ops"));
    assert!(block.contains("- publish: Publish a video"));
    assert!(block.contains("plugin.search_tools"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo test --lib tools_builder::tests::catalog`
Expected: FAIL — `plugin_tool_list` arity/signature mismatch and `render_plugin_catalog_block` signature mismatch.

- [ ] **Step 3: Replace `plugin_tool_examples` and `render_plugin_catalog_block`**

Replace the existing `plugin_tool_examples` fn and `render_plugin_catalog_block` fn with:

```rust
/// Render a plugin's tools as `- name: one-line purpose` lines for the catalog.
/// Tools named in `common` come first (in `common` order); the rest follow
/// sorted by name. Output is capped at `cap`; the remainder is summarised with
/// a pointer to plugin.search_tools. Byte-stable for a given input (KV-cache
/// hygiene in the per-machine layer). `tools` is (name, description) pairs.
fn plugin_tool_list(tools: &[(&str, &str)], common: &[String], cap: usize) -> String {
    if tools.is_empty() {
        return "  (no declared tools)".to_owned();
    }
    let is_common = |n: &str| common.iter().any(|c| c == n);
    // Common first, in declared order (only those that actually exist).
    let mut ordered: Vec<(&str, &str)> = common
        .iter()
        .filter_map(|c| tools.iter().find(|(n, _)| n == c).copied())
        .collect();
    // Then the rest, sorted by name.
    let mut rest: Vec<(&str, &str)> = tools.iter().filter(|(n, _)| !is_common(n)).copied().collect();
    rest.sort_unstable_by(|a, b| a.0.cmp(b.0));
    ordered.extend(rest);

    let total = ordered.len();
    let mut lines: Vec<String> = ordered
        .iter()
        .take(cap)
        .map(|(n, d)| {
            let d = d.trim();
            if d.is_empty() {
                format!("- {n}")
            } else {
                let short: String = d.chars().take(100).collect();
                format!("- {n}: {}", short.trim_end())
            }
        })
        .collect();
    if total > cap {
        let n = total - cap;
        let noun = if n == 1 { "tool" } else { "tools" };
        lines.push(format!("- …{n} more {noun} — plugin.search_tools to find them"));
    }
    lines.join("\n")
}

/// Render one plugin's catalog block, SKILL.md-style: heading with summary,
/// description, then common tools. `summary_or_desc` is the manifest summary
/// (falls back to description upstream). `tools` is (name, description) pairs.
fn render_plugin_catalog_block(
    name: &str,
    version: &str,
    summary_or_desc: &str,
    tools: &[(&str, &str)],
    common: &[String],
    cap: usize,
) -> String {
    let list = plugin_tool_list(tools, common, cap);
    format!(
        "### {name} — {summary_or_desc} (v{version})\n\
         Common tools (call via plugin.invoke {{plugin:\"{name}\", tool, arguments}}; \
         use plugin.search_tools {{plugin:\"{name}\", query}} for others):\n\
         {list}"
    )
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo test --lib tools_builder::tests::catalog`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add src/agent/tools_builder.rs
git commit -m "feat(prompt): SKILL.md-style plugin catalog renderer (summary + common tools)"
```

---

### Task 4: Wire `build_plugins_system` to the new renderer

**Files:**
- Modify: `src/agent/tools_builder.rs` (`build_plugins_system` wasm + js call sites; section header)

- [ ] **Step 1: Replace the WASM-plugin map block**

In `build_plugins_system`, the WASM `.map(|p| { ... })` closure becomes:

```rust
        .map(|p| {
            let tools: Vec<(&str, &str)> =
                p.tools.iter().map(|t| (t.name.as_str(), t.description.as_str())).collect();
            let blurb = p
                .summary
                .as_deref()
                .or(p.description.as_deref())
                .unwrap_or("");
            (
                p.name.clone(),
                render_plugin_catalog_block(
                    &p.name,
                    p.version.as_deref().unwrap_or(""),
                    blurb,
                    &tools,
                    &p.common_tools,
                    8,
                ),
            )
        })
```

- [ ] **Step 2: Replace the JS-plugin loop block**

The JS `for (plugin_name, plugin) in reg.js_plugins_iter()` body becomes:

```rust
        for (plugin_name, plugin) in reg.js_plugins_iter() {
            let m = &plugin.manifest;
            let tools: Vec<(&str, &str)> =
                m.tools.iter().map(|t| (t.name.as_str(), t.description.as_str())).collect();
            let blurb = m.summary.as_deref().or(m.description.as_deref()).unwrap_or("");
            blocks.push((
                plugin_name.clone(),
                render_plugin_catalog_block(
                    plugin_name,
                    m.version.as_deref().unwrap_or(""),
                    blurb,
                    &tools,
                    &m.common_tools,
                    8,
                ),
            ));
        }
```

- [ ] **Step 3: Update the section header** so the intro matches the new per-plugin format. Replace the `Some(format!(...))` tail of `build_plugins_system` with:

```rust
    Some(format!(
        "## Installed Plugins\n\
         Each plugin bundles many tools. The common ones are listed below; \
         call them with `plugin.invoke`, and use `plugin.search_tools` \
         {{plugin, query}} to find any not listed. Prefer a plugin over a \
         generic browser flow when it covers the task.\n\n\
         {}",
        blocks_text.join("\n\n"),
    ))
```

- [ ] **Step 4: Build + run the full tools_builder test module**

Run: `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo test --lib tools_builder`
Expected: PASS (catalog tests + any existing tools_builder tests; fix any test that referenced the old `render_plugin_catalog_block` signature, e.g. the `plugin_tool_examples([...])` test around line 1935 — update it to the new `plugin_tool_list`/`render_plugin_catalog_block` signatures or delete if redundant with Task 3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/agent/tools_builder.rs
git commit -m "feat(prompt): render installed-plugins catalog from summary + common tools"
```

---

### Task 5: Shared-prefix guidance + CAPABILITY PRIORITY rewrite

**Files:**
- Modify: `src/agent/prompt_builder.rs` (`build_shared_system_prefix`, the `CAPABILITY PRIORITY` `parts.push(...)`)

This changes `shared_prefix` → drifts the frozen baseline (handled in Task 6).

- [ ] **Step 1: Rewrite the "How to invoke an installed plugin" subsection**

Replace the `### How to invoke an installed plugin` block (the `When a task matches "## Installed Plugins": 1. Call plugin.search_tools ...` lines) with:

```rust
         ### How to use plugins\n\
         Installed plugins are listed in the \"## Installed Plugins\" section \
         below, each with its common tools.\n\
         1. If a listed tool fits, call `plugin.describe_tool` {plugin, tool} \
         for exact arguments (when needed), then `plugin.invoke`.\n\
         2. If no listed tool fits, call `plugin.search_tools` with the plugin \
         name (from the list) and the user's intent to find one, then \
         describe/invoke.\n\
         3. `plugin.invoke` with `{plugin, tool, arguments}`. If a WASM and JS \
         plugin expose the same capability, choose WASM.\n\n\
```

- [ ] **Step 2: Rewrite the CAPABILITY PRIORITY ranking**

In the same `parts.push` string, replace the numbered ranking (items 1–4 and the "Built-in tools — fallback ONLY ..." line) with a fit-based framing. Replace the block from `For every user request, evaluate sources ...` through item 4 with:

```rust
         For every user request, choose the capability that best FITS the \
         task — do not rank by source type:\n\
         - **Plugins** (\"## Installed Plugins\") and **skills** (\"## Installed \
         Skills\") cover specific domains (flights, stocks, a marketplace, …). \
         If a plugin/skill description matches the user's intent, prefer it \
         over a generic browser/web flow.\n\
         - **Built-in tools** (`computer_use`, `web_browser`, `web_fetch`, \
         `shell`, `read_file`, …) are first-class for what they do. Use the \
         built-in that is the right tool for the job; a plugin/skill does NOT \
         outrank a built-in that already fits.\n\
         - When a WASM and JS plugin both fit, choose WASM.\n\n\
```

(Keep the existing "Common failure mode" example and the "### How to invoke an installed skill" subsection unchanged.)

- [ ] **Step 3: Build to verify it compiles**

Run: `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo build --lib`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add src/agent/prompt_builder.rs
git commit -m "feat(prompt): fit-based capability priority + clearer plugin-usage guidance"
```

---

### Task 6: Re-export the frozen baseline fixture

**Files:**
- Modify: `tests/fixtures/baseline-2026.5.20.json` (regenerated)

Tasks 1–4 are per-machine (no base-layer change). Task 5 changed `shared_prefix`, so the `rsclaw/2026.5.20` baseline must be re-exported.

- [ ] **Step 1: Confirm the baseline currently fails (proves Task 5 is the only base-layer drift)**

Run: `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo test --test baseline_2026_5_20`
Expected: FAIL on `baseline_shared_prefix_byte_stable` (length/content drift); `builtin_tools` test still PASSES (façade unchanged → tools list identical).

- [ ] **Step 2: Re-export the fixture**

Run:
```bash
RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test \
  cargo run -q -- debug dump-prompt-spec --shared-only -o tests/fixtures/baseline-2026.5.20.json
```
Expected: `wrote tests/fixtures/baseline-2026.5.20.json (...)`.

- [ ] **Step 3: Verify the baseline passes**

Run: `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo test --test baseline_2026_5_20`
Expected: PASS (3 passed, 1 ignored).

- [ ] **Step 4: Commit**

```bash
git add tests/fixtures/baseline-2026.5.20.json
git commit -m "test(baseline): re-export 2026.5.20 fixture for capability-priority + plugin guidance"
```

> NOTE for the operator: this changes the `rsclaw/2026.5.20` base-layer KV hash. rsclaw-llm workers must re-ingest the new shared_prefix (the SHA-256 of shared_prefix + builtin_tools is the canonical identifier) or pre-registered cache slots stop hitting for this gateway version.

---

## Final verification

- [ ] Run `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo test --lib plugin:: tools_builder` and `cargo test --test baseline_2026_5_20` — all pass.
- [ ] Run `RSCLAW_BUILD_VERSION=2026.5.20 RSCLAW_BUILD_DATE=test cargo clippy --lib` — no new warnings.
