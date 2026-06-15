//! Plugin catalog, selection, and invocation tool helpers.

use anyhow::Result;
use serde_json::{Value, json};

use super::{
    context_mgr::estimate_tokens,
    runtime::{
        AgentRuntime, PLUGIN_TOOL_SEP, PluginInjectResolution, PluginOverride, PluginToolInfo,
        PluginUserToolSelection, RunContext, bucket_qualified_names,
    },
};

/// Format one active-plugin-tool line for the "## Active Plugin Tools"
/// block. Schema is serialized compactly via `serde_json` so a 20-tool
/// block stays under ~3-4 KB. The schema JSON object's keys are emitted
/// in `serde_json` declaration order (the crate is compiled with
/// `preserve_order`), so byte-stability for a given input is up to the
/// upstream tool registry — not normalized here because this block lives
/// in `user_system` (NOT hashed), so cross-session byte-divergence has
/// no KV-cache impact.
#[allow(dead_code)]
fn format_active_plugin_tool_line(name: &str, description: &str, schema: &Value) -> String {
    let one_line_desc = description.trim().replace('\n', " ");
    let schema_compact = serde_json::to_string(schema)
        .unwrap_or_else(|_| r#"{"type":"object","properties":{}}"#.to_owned());
    format!("- **{name}** — {one_line_desc}\n  input_schema: {schema_compact}")
}

impl AgentRuntime {
    fn collect_plugin_tools(&self) -> Vec<PluginToolInfo> {
        let mut out = Vec::new();
        for plugin in self.wasm_plugins.iter() {
            for tool in &plugin.tools {
                out.push(PluginToolInfo {
                    plugin: plugin.name.clone(),
                    runtime: "wasm",
                    tool: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.parameters.clone(),
                });
            }
        }
        if let Some(reg) = self.plugins.as_ref() {
            for (plugin_name, plugin) in reg.js_plugins_iter() {
                for tool in &plugin.manifest.tools {
                    out.push(PluginToolInfo {
                        plugin: plugin_name.clone(),
                        runtime: "js",
                        tool: tool.name.clone(),
                        description: tool.description.clone(),
                        input_schema: tool
                            .input_schema
                            .clone()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    });
                }
            }
        }
        out
    }

    fn find_plugin_tool(&self, plugin: &str, tool: &str) -> Option<PluginToolInfo> {
        self.collect_plugin_tools()
            .into_iter()
            .find(|t| t.plugin == plugin && t.tool == tool)
    }

    fn compact_input_schema(schema: &Value) -> Value {
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut props = serde_json::Map::new();
        if let Some(obj) = schema.get("properties").and_then(|v| v.as_object()) {
            for (name, raw) in obj.iter().take(16) {
                let mut compact = serde_json::Map::new();
                if let Some(t) = raw.get("type") {
                    compact.insert("type".to_owned(), t.clone());
                }
                if let Some(en) = raw.get("enum") {
                    compact.insert("enum".to_owned(), en.clone());
                }
                if let Some(default) = raw.get("default") {
                    compact.insert("default".to_owned(), default.clone());
                }
                if let Some(desc) = raw.get("description").and_then(|d| d.as_str()) {
                    compact.insert(
                        "description".to_owned(),
                        json!(rsclaw_util::truncate_str(desc, 180)),
                    );
                }
                props.insert(name.clone(), Value::Object(compact));
            }
        }
        json!({
            "required": required,
            "properties": props,
        })
    }

    fn plugin_tool_summary(tool: &PluginToolInfo) -> Value {
        json!({
            "plugin": tool.plugin,
            "tool": tool.tool,
            "name": format!("{}.{}", tool.plugin, tool.tool),
            "runtime": tool.runtime,
            "description": rsclaw_util::truncate_str(&tool.description, 280),
            "input_schema_compact": Self::compact_input_schema(&tool.input_schema),
        })
    }

    fn plugin_runtime_priority(runtime: &str) -> u8 {
        match runtime {
            "wasm" => 0,
            "js" => 1,
            _ => 2,
        }
    }

    fn is_cjk_char(ch: char) -> bool {
        matches!(
            ch,
            '\u{3400}'..='\u{4DBF}'
                | '\u{4E00}'..='\u{9FFF}'
                | '\u{F900}'..='\u{FAFF}'
                | '\u{3040}'..='\u{30FF}'
                | '\u{AC00}'..='\u{D7AF}'
        )
    }

    fn score_plugin_tool(query: &str, tool: &PluginToolInfo) -> i32 {
        let query_l = query.to_lowercase();
        let haystack = format!(
            "{} {} {} {}",
            tool.plugin,
            tool.tool,
            tool.tool.replace('_', " "),
            tool.description
        )
        .to_lowercase();

        let mut score = 0;
        if !query_l.is_empty() && haystack.contains(&query_l) {
            score += 30;
        }
        for token in query_l
            .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '，' | '；' | ':' | '：'))
            .filter(|t| !t.is_empty())
        {
            if tool.tool.to_lowercase().contains(token) {
                score += 12;
            }
            if tool.description.to_lowercase().contains(token) {
                score += 6;
            }
            if tool.plugin.to_lowercase().contains(token) {
                score += 3;
            }
        }

        let mut cjk_total = 0;
        let mut cjk_hits = 0;
        for ch in query_l.chars().filter(|ch| Self::is_cjk_char(*ch)) {
            cjk_total += 1;
            if haystack.contains(ch) {
                cjk_hits += 1;
            }
        }
        if cjk_total > 0 {
            score += cjk_hits * 4;
            if cjk_hits == cjk_total {
                score += 10;
            }
        }
        score
    }

    /// Render an "## Active Plugin Tools" markdown block listing every
    /// `<plugin>.<tool>` selected by `per_plugin`, with the full
    /// `description` and `input_schema`.
    ///
    /// **Superseded by `select_user_tools_pure` as of v1.9** — plugin
    /// tools now flow into `dynamic_prefix.user_tools` as real
    /// structured ToolDefs (their own cache segment), so re-rendering
    /// the same data as `user_system` text would be duplicate work
    /// and waste prompt tokens. Kept for future debug-mode use
    /// (e.g. `/plugin describe <plugin> --text`) and to keep the
    /// resolver primitives importable from tests; not on any live
    /// turn-build path.
    #[allow(dead_code)]
    pub(crate) fn render_active_plugin_tools_text_pure(
        wasm_plugins: &[rsclaw_plugin::wasm_runtime::WasmPlugin],
        js_plugins: Option<&rsclaw_plugin::PluginRegistry>,
        per_plugin: &std::collections::HashMap<String, PluginOverride>,
        cap: usize,
    ) -> Option<String> {
        if per_plugin.is_empty() {
            return None;
        }
        let mut blocks: Vec<String> = Vec::new();
        let mut emitted: usize = 0;
        // WASM first (priority over JS — matches dispatch_tool's
        // wasm-wins ordering for the `<plugin>.<tool>` namespace).
        for wp in wasm_plugins {
            if emitted >= cap {
                break;
            }
            let names = match Self::resolve_plugin_inject_pure(per_plugin, &wp.name) {
                PluginInjectResolution::None => continue,
                PluginInjectResolution::All => wp.tools.iter().map(|t| t.name.clone()).collect(),
                PluginInjectResolution::Names(v) => v,
            };
            let mut lines: Vec<String> = Vec::new();
            for name in &names {
                if emitted >= cap {
                    break;
                }
                if let Some(t) = wp.tools.iter().find(|t| &t.name == name) {
                    lines.push(format_active_plugin_tool_line(
                        &t.name,
                        &t.description,
                        &t.parameters,
                    ));
                    emitted += 1;
                }
            }
            if !lines.is_empty() {
                blocks.push(format!("### {} (wasm)\n{}", wp.name, lines.join("\n")));
            }
        }
        if let Some(reg) = js_plugins {
            for (plugin_name, plugin) in reg.js_plugins_iter() {
                if emitted >= cap {
                    break;
                }
                let names = match Self::resolve_plugin_inject_pure(per_plugin, plugin_name) {
                    PluginInjectResolution::None => continue,
                    PluginInjectResolution::All => plugin
                        .manifest
                        .tools
                        .iter()
                        .map(|t| t.name.clone())
                        .collect(),
                    PluginInjectResolution::Names(v) => v,
                };
                let mut lines: Vec<String> = Vec::new();
                for name in &names {
                    if emitted >= cap {
                        break;
                    }
                    if let Some(t) = plugin.manifest.tools.iter().find(|t| &t.name == name) {
                        let schema = t
                            .input_schema
                            .clone()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                        lines.push(format_active_plugin_tool_line(
                            &t.name,
                            &t.description,
                            &schema,
                        ));
                        emitted += 1;
                    }
                }
                if !lines.is_empty() {
                    blocks.push(format!("### {plugin_name} (js)\n{}", lines.join("\n")));
                }
            }
        }
        if blocks.is_empty() {
            return None;
        }
        Some(format!(
            "## Active Plugin Tools\n\
             These plugin tools are activated for this session. Call each via \
             `plugin_invoke {{plugin, tool, arguments}}` — `arguments` MUST match \
             the `input_schema` below. The host validates required fields before \
             dispatch.\n\n\
             {}",
            blocks.join("\n\n"),
        ))
    }

    /// Render the active-plugins block for this agent's CONFIG-declared
    /// `model.plugin_tools` list (always-on, agent-level).
    ///
    /// **Dead since v1.9**: superseded by `select_user_tools_pure`,
    /// which feeds the same tools into `dynamic_prefix.user_tools` as
    /// real ToolDefs instead of `user_system` text. Kept under
    /// `#[allow(dead_code)]` so a future debug command (`/plugin
    /// inspect`) can call the rendering primitive without us having
    /// to rewrite the wrapper.
    #[allow(dead_code)]
    pub(crate) fn render_config_plugin_tools_text(&self) -> Option<String> {
        const MAX_INJECT_TOOLS: usize = 20;
        let model_cfg = self.handle.config.model.as_ref()?;
        let list = model_cfg.plugin_tools.as_ref()?;
        if list.is_empty() {
            return None;
        }
        let mut map: std::collections::HashMap<String, PluginOverride> =
            std::collections::HashMap::new();
        for entry in list {
            // Accept both `plugin.tool` (preferred) and `plugin/tool` for
            // operators who muscle-memory from skill paths.
            let Some((plugin, tool)) = entry.split_once('.').or_else(|| entry.split_once('/'))
            else {
                tracing::warn!(
                    agent = %self.handle.id,
                    entry = %entry,
                    "plugin_tools entry must be '<plugin>.<tool>'; skipping"
                );
                continue;
            };
            map.entry(plugin.to_owned())
                .or_default()
                .inject
                .push(tool.to_owned());
        }
        Self::render_active_plugin_tools_text_pure(
            &self.wasm_plugins,
            self.plugins.as_deref(),
            &map,
            MAX_INJECT_TOOLS,
        )
    }

    /// Render the active-plugins block for the per-session `/plugin`
    /// slash command overrides. Returns `None` when no override is set.
    ///
    /// **Dead since v1.9**: see `render_active_plugin_tools_text_pure`
    /// docstring for the migration story. The slash-command-driven
    /// overrides now flow through `select_user_tools_pure` and land
    /// in `dynamic_prefix.user_tools`.
    #[allow(dead_code)]
    pub(crate) fn render_session_plugin_tools_text(&self, session_key: &str) -> Option<String> {
        /// Max plugin tools injected per turn (v1 hard cap; v2 swaps for
        /// token-budget). Keeps small-model prompt size under control even
        /// when the user `/plugin xxx all`s a 200-tool plugin.
        const MAX_INJECT_TOOLS: usize = 20;
        let snapshot = match self.handle.plugin_overrides.read() {
            Ok(g) => g.get(session_key).cloned().unwrap_or_default(),
            Err(_) => return None,
        };
        if snapshot.is_empty() {
            return None;
        }
        Self::render_active_plugin_tools_text_pure(
            &self.wasm_plugins,
            self.plugins.as_deref(),
            &snapshot,
            MAX_INJECT_TOOLS,
        )
    }

    /// Pure resolver — what `<plugin>.<tool>` ToolDefs should we inject for
    /// this (session, plugin)? Returns either `Vec::new()` (default / disabled)
    /// or the explicit tool name list. `None` means "expand to all plugin
    /// tools at the call site" (the caller has the plugin metadata).
    ///
    /// Separated from the live-lookup path so unit tests can drive it
    /// without locking the `AgentHandle`'s RwLock.
    pub(crate) fn resolve_plugin_inject_pure(
        per_plugin: &std::collections::HashMap<String, PluginOverride>,
        plugin: &str,
    ) -> PluginInjectResolution {
        match per_plugin.get(plugin) {
            Some(o) if o.disabled => PluginInjectResolution::None,
            Some(o) if o.inject_all => PluginInjectResolution::All,
            Some(o) => PluginInjectResolution::Names(o.inject.clone()),
            None => PluginInjectResolution::None,
        }
    }

    /// Compute the set of plugin tools to expose as real ToolDefs in
    /// `dynamic_prefix.user_tools` for this turn.
    ///
    /// Selection layers (later layers win):
    /// 1. **headline default** — `headline: true` in the plugin's
    ///    `plugin.json5` is the plugin-author-declared baseline. If the session
    ///    override has `inject_all: true` this is replaced by the plugin's full
    ///    tool list; if it has a non-empty `inject`, this is replaced by that
    ///    list.
    /// 2. **per-agent pin** — names in `config_pin` (sourced from
    ///    `model.plugin_tools`) get added on top of the base, even when not
    ///    `headline`-marked.
    /// 3. **session pin** — `PluginOverride.pin` adds for one session (slash
    ///    command `/plugin pin <plugin>__<tool>`).
    /// 4. **per-agent unpin** — `config_unpin` (`model.plugin_tools_unpin`)
    ///    removes names from the resolved base, even headlines.
    /// 5. **session unpin** — `PluginOverride.unpin` removes for one session
    ///    (`/plugin unpin <plugin>__<tool>`).
    ///
    /// Cap is applied last, plugin-by-plugin in declared order
    /// (WASM first, then JS — matches dispatch priority). Excess
    /// tools stay reachable via the `plugin_invoke` meta-tool at
    /// zero prompt-token cost.
    ///
    /// Pure / no I/O — driven entirely by inputs so unit tests can
    /// exercise it without the runtime state.
    pub(crate) fn select_user_tools_pure(
        wasm_plugins: &[rsclaw_plugin::wasm_runtime::WasmPlugin],
        js_plugins: Option<&rsclaw_plugin::PluginRegistry>,
        per_plugin: &std::collections::HashMap<String, PluginOverride>,
        config_pin: &[String],
        config_unpin: &[String],
        // v2 toolGroups: pinned groups as "<plugin>:<group>" (config
        // `model.pluginGroups` ∪ session `request_tool` enables). Members
        // are included as if headline-tagged.
        group_pins: &std::collections::BTreeSet<String>,
        cap: usize,
        // v2: token budget. When Some, admission is by summed token
        // estimate instead of count — greedy in manifest order, stops at
        // the first selection that would cross the line (deterministic).
        budget: Option<usize>,
    ) -> Vec<PluginUserToolSelection> {
        let mut spent_tokens: usize = 0;
        // Bucket config-level pin/unpin by plugin so per-plugin lookup
        // is O(active tools) instead of O(config_entries × active_tools).
        let config_pin_by_plugin = bucket_qualified_names(config_pin);
        let config_unpin_by_plugin = bucket_qualified_names(config_unpin);

        let mut selected: Vec<PluginUserToolSelection> = Vec::new();

        // Inner helper closure: collect from one plugin's tool list,
        // honoring all five selection layers.
        let mut take_from_plugin =
            |plugin_name: &str,
             tool_iter: Vec<(String, String, Value, bool, Option<String>)>|
             -> bool {
                // Returns `false` when cap was reached and the caller
                // should stop iterating further plugins.
                let session = per_plugin.get(plugin_name);
                if session.map(|o| o.disabled).unwrap_or(false) {
                    return true;
                }

                // Build the candidate set as an ordered name list so the
                // wire output is stable across runs (HashMap iteration is
                // not). Walk the plugin's declared tools in manifest order
                // and decide inclusion.
                let mut base_names: std::collections::BTreeSet<String> =
                    if session.map(|o| o.inject_all).unwrap_or(false) {
                        tool_iter.iter().map(|(n, _, _, _, _)| n.clone()).collect()
                    } else if let Some(o) = session.filter(|o| !o.inject.is_empty()) {
                        o.inject.iter().cloned().collect()
                    } else {
                        tool_iter
                            .iter()
                            .filter(|(_, _, _, hl, _)| *hl)
                            .map(|(n, _, _, _, _)| n.clone())
                            .collect()
                    };
                // v2: members of pinned groups join the base set.
                if !group_pins.is_empty() {
                    for (n, _, _, _, g) in &tool_iter {
                        if let Some(g) = g
                            && group_pins.contains(&format!("{plugin_name}:{g}"))
                        {
                            base_names.insert(n.clone());
                        }
                    }
                }

                // pin: add (config + session)
                let mut effective = base_names;
                if let Some(set) = config_pin_by_plugin.get(plugin_name) {
                    for name in set {
                        effective.insert(name.clone());
                    }
                }
                if let Some(o) = session {
                    for name in &o.pin {
                        effective.insert(name.clone());
                    }
                }

                // unpin: subtract (config + session). Subtract LAST so
                // unpin always wins a tie with pin.
                if let Some(set) = config_unpin_by_plugin.get(plugin_name) {
                    for name in set {
                        effective.remove(name);
                    }
                }
                if let Some(o) = session {
                    for name in &o.unpin {
                        effective.remove(name);
                    }
                }

                // Emit in manifest order so the wire bytes are stable.
                for (name, description, input_schema, _, group) in tool_iter {
                    if !effective.contains(&name) {
                        continue;
                    }
                    if let Some(b) = budget {
                        let cost = estimate_tokens(&name)
                            + estimate_tokens(&description)
                            + estimate_tokens(&input_schema.to_string());
                        if spent_tokens + cost > b {
                            return false;
                        }
                        spent_tokens += cost;
                    } else if selected.len() >= cap {
                        return false;
                    }
                    selected.push(PluginUserToolSelection {
                        plugin_name: plugin_name.to_owned(),
                        tool_name: name,
                        description,
                        input_schema,
                        group,
                    });
                }
                true
            };

        // WASM first (matches dispatch_tool's wasm-wins ordering).
        for wp in wasm_plugins {
            let tools: Vec<(String, String, Value, bool, Option<String>)> = wp
                .tools
                .iter()
                .map(|t| {
                    (
                        t.name.clone(),
                        t.description.clone(),
                        t.parameters.clone(),
                        t.headline,
                        t.group.clone(),
                    )
                })
                .collect();
            if !take_from_plugin(&wp.name, tools) {
                return selected;
            }
        }
        if let Some(reg) = js_plugins {
            for (plugin_name, plugin) in reg.js_plugins_iter() {
                let tools: Vec<(String, String, Value, bool, Option<String>)> = plugin
                    .manifest
                    .tools
                    .iter()
                    .map(|t| {
                        let schema = t
                            .input_schema
                            .clone()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                        (
                            t.name.clone(),
                            t.description.clone(),
                            schema,
                            t.headline,
                            t.group.clone(),
                        )
                    })
                    .collect();
                if !take_from_plugin(plugin_name, tools) {
                    return selected;
                }
            }
        }
        selected
    }

    /// v2 toolGroups: collect grouped plugin tools that did NOT make the
    /// live selection, as `(enable_key, ToolDef)` pairs ready for the
    /// `request_tool` same-turn splice, plus one stub line per offerable
    /// group ("plugin:group — desc (N tools)"). Enable keys are
    /// namespaced `pg:<plugin>:<group>` to share the cold_enabled session
    /// map with builtin cold tools without collisions.
    pub(crate) fn collect_deferred_group_tools(
        wasm_plugins: &[rsclaw_plugin::wasm_runtime::WasmPlugin],
        js_plugins: Option<&rsclaw_plugin::PluginRegistry>,
        live: &[PluginUserToolSelection],
    ) -> (
        Vec<(String, rsclaw_provider::ToolDef)>,
        Vec<(String, String)>,
    ) {
        use std::collections::{BTreeMap, HashSet};
        let live_names: HashSet<(&str, &str)> = live
            .iter()
            .map(|s| (s.plugin_name.as_str(), s.tool_name.as_str()))
            .collect();
        let mut defs: Vec<(String, rsclaw_provider::ToolDef)> = Vec::new();
        // group key → (group desc, member count not live)
        let mut groups: BTreeMap<String, (String, usize)> = BTreeMap::new();

        let mut visit =
            |plugin: &str,
             group_meta: &std::collections::HashMap<String, String>,
             tools: Vec<(String, String, Value, Option<String>)>| {
                for (name, description, schema, group) in tools {
                    let Some(g) = group else { continue };
                    if live_names.contains(&(plugin, name.as_str())) {
                        continue;
                    }
                    let key = format!("{plugin}:{g}");
                    let entry = groups
                        .entry(key.clone())
                        .or_insert_with(|| (group_meta.get(&g).cloned().unwrap_or_default(), 0));
                    entry.1 += 1;
                    defs.push((
                        format!("pg:{key}"),
                        rsclaw_provider::ToolDef {
                            name: format!("{plugin}{PLUGIN_TOOL_SEP}{name}"),
                            description,
                            parameters: schema,
                        },
                    ));
                }
            };

        for wp in wasm_plugins {
            let tools = wp
                .tools
                .iter()
                .map(|t| {
                    (
                        t.name.clone(),
                        t.description.clone(),
                        t.parameters.clone(),
                        t.group.clone(),
                    )
                })
                .collect();
            visit(&wp.name, &wp.tool_groups, tools);
        }
        if let Some(reg) = js_plugins {
            for (plugin_name, plugin) in reg.js_plugins_iter() {
                let tools = plugin
                    .manifest
                    .tools
                    .iter()
                    .map(|t| {
                        (
                            t.name.clone(),
                            t.description.clone(),
                            t.input_schema
                                .clone()
                                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                            t.group.clone(),
                        )
                    })
                    .collect();
                visit(plugin_name, &plugin.manifest.tool_groups, tools);
            }
        }

        let lines = groups
            .into_iter()
            .map(|(key, (desc, n))| {
                let label = if desc.is_empty() {
                    format!("{key} ({n} tools)")
                } else {
                    format!("{key} — {desc} ({n} tools)")
                };
                (key, label)
            })
            .collect();
        (defs, lines)
    }

    /// Set or update a plugin override for a session. Called by the `/plugin`
    /// slash command handler via `&AgentHandle`.
    pub fn set_plugin_override(
        handle: &crate::AgentHandle,
        session_key: &str,
        plugin: &str,
        override_: PluginOverride,
    ) {
        if let Ok(mut g) = handle.plugin_overrides.write() {
            g.entry(session_key.to_owned())
                .or_default()
                .insert(plugin.to_owned(), override_);
        }
    }

    /// Remove all plugin overrides for a session (e.g. `/plugin reset`).
    pub fn clear_plugin_overrides(handle: &crate::AgentHandle, session_key: &str) {
        if let Ok(mut g) = handle.plugin_overrides.write() {
            g.remove(session_key);
        }
        // Cold-tool re-enables are session state too — /clear resets them.
        if let Ok(mut g) = handle.cold_enabled.write() {
            g.remove(session_key);
        }
    }

    /// In-place mutate the session override for `(session_key, plugin)`,
    /// creating a default entry if none exists. Used by additive
    /// commands (`/plugin pin` and `/plugin unpin`) that modify a single
    /// field without replacing the whole override the way
    /// `set_plugin_override` does. Holds the write lock across the
    /// closure — keep the closure cheap.
    pub fn mutate_plugin_override<F>(
        handle: &crate::AgentHandle,
        session_key: &str,
        plugin: &str,
        mutate: F,
    ) where
        F: FnOnce(&mut PluginOverride),
    {
        if let Ok(mut g) = handle.plugin_overrides.write() {
            let plugin_map = g.entry(session_key.to_owned()).or_default();
            mutate(plugin_map.entry(plugin.to_owned()).or_default());
        }
    }

    pub(crate) async fn tool_plugin_info(&self, args: Value) -> Result<Value> {
        let plugin_filter = args["plugin"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut by_plugin =
            std::collections::BTreeMap::<String, (&'static str, Vec<PluginToolInfo>)>::new();
        for tool in self.collect_plugin_tools() {
            if plugin_filter.is_none_or(|p| tool.plugin == p) {
                by_plugin
                    .entry(tool.plugin.clone())
                    .or_insert((tool.runtime, Vec::new()))
                    .1
                    .push(tool);
            }
        }

        let plugins = by_plugin
            .into_iter()
            .map(|(plugin, (runtime, mut tools))| {
                tools.sort_by(|a, b| a.tool.cmp(&b.tool));
                let common_tools = tools
                    .iter()
                    .take(12)
                    .map(|tool| {
                        json!({
                            "tool": tool.tool,
                            "name": format!("{}.{}", plugin, tool.tool),
                            "description": rsclaw_util::truncate_str(&tool.description, 180),
                        })
                    })
                    .collect::<Vec<_>>();
                json!({
                    "plugin": plugin,
                    "runtime": runtime,
                    "tool_count": tools.len(),
                    "common_tools": common_tools,
                })
            })
            .collect::<Vec<_>>();

        Ok(json!({
            "plugin": plugin_filter,
            "plugins": plugins,
            "next_steps": [
                "Use plugin_search to find task-specific tools.",
                "Use plugin_describe to inspect exact input schema.",
                "Use plugin_invoke to execute a plugin tool."
            ]
        }))
    }

    pub(crate) async fn tool_plugin_search_tools(&self, args: Value) -> Result<Value> {
        Ok(Self::search_plugin_tools_pure(
            self.collect_plugin_tools(),
            &args,
        ))
    }

    /// Pure resolver — same logic as the tool dispatch, but takes the tool
    /// list as input so unit tests can drive it without constructing a full
    /// `AgentRuntime`. Two modes:
    /// - **search**: non-empty `query` → score + rank + paginate.
    /// - **browse**: empty `query` + non-empty `plugin` → list-all
    ///   alphabetical, paginated via `offset`/`limit`. Lets the model walk a
    ///   giant plugin (e.g. douyin's 208 tools) without inventing a new
    ///   meta-tool.
    pub(crate) fn search_plugin_tools_pure(all_tools: Vec<PluginToolInfo>, args: &Value) -> Value {
        let query = args["query"].as_str().unwrap_or("").trim();
        let plugin_filter = args["plugin"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let limit = args["limit"].as_u64().unwrap_or(8).clamp(1, 50) as usize;
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;

        if query.is_empty() && plugin_filter.is_none() {
            return json!({
                "error": "plugin_search requires either `query` (search) or `plugin` (browse).",
                "hint": "Examples: {plugin:\"douyin\",query:\"publish video\"} or {plugin:\"douyin\"} to list all tools."
            });
        }

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
            let next_offset = if offset + page.len() < total {
                json!(offset + page.len())
            } else {
                Value::Null
            };
            return json!({
                "plugin": plugin,
                "mode": "list",
                "total": total,
                "offset": offset,
                "limit": limit,
                "tools": page,
                "next_offset": next_offset,
            });
        }

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
        let next_offset = if offset + page.len() < total {
            json!(offset + page.len())
        } else {
            Value::Null
        };

        json!({
            "query": query,
            "plugin": plugin_filter,
            "mode": "search",
            "total": total,
            "offset": offset,
            "limit": limit,
            "tools": page,
            "next_offset": next_offset,
        })
    }

    pub(crate) async fn tool_plugin_describe_tool(&self, args: Value) -> Result<Value> {
        let plugin = args["plugin"].as_str().unwrap_or("").trim();
        let tool = args["tool"].as_str().unwrap_or("").trim();
        if plugin.is_empty() || tool.is_empty() {
            return Ok(json!({"error": "plugin_describe requires plugin and tool"}));
        }
        let Some(info) = self.find_plugin_tool(plugin, tool) else {
            return Ok(json!({
                "error": format!("plugin tool not found: {plugin}.{tool}"),
                "hint": "Use plugin_search to discover installed plugin tools."
            }));
        };
        Ok(json!({
            "plugin": info.plugin,
            "tool": info.tool,
            "name": format!("{}.{}", info.plugin, info.tool),
            "runtime": info.runtime,
            "description": info.description,
            "input_schema": info.input_schema,
        }))
    }

    fn validate_plugin_arguments(
        tool: &PluginToolInfo,
        args: &Value,
    ) -> std::result::Result<(), Value> {
        if !args.is_object() {
            return Err(json!({
                "error": "plugin_invoke arguments must be an object",
                "plugin": tool.plugin,
                "tool": tool.tool,
                "schema_hint": Self::compact_input_schema(&tool.input_schema),
            }));
        }
        if let Some(required) = tool.input_schema.get("required").and_then(|v| v.as_array()) {
            let missing: Vec<&str> = required
                .iter()
                .filter_map(|v| v.as_str())
                .filter(|key| args.get(*key).is_none_or(Value::is_null))
                .collect();
            if !missing.is_empty() {
                return Err(json!({
                    "error": "plugin_invoke missing required arguments",
                    "plugin": tool.plugin,
                    "tool": tool.tool,
                    "missing": missing,
                    "schema_hint": Self::compact_input_schema(&tool.input_schema),
                }));
            }
        }
        Ok(())
    }

    pub(crate) async fn tool_plugin_invoke(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let plugin_name = args["plugin"].as_str().unwrap_or("").trim();
        let tool_name = args["tool"].as_str().unwrap_or("").trim();
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
        if plugin_name.is_empty() || tool_name.is_empty() {
            return Ok(json!({"error": "plugin_invoke requires plugin, tool, and arguments"}));
        }
        let Some(info) = self.find_plugin_tool(plugin_name, tool_name) else {
            return Ok(json!({
                "error": format!("plugin tool not found: {plugin_name}.{tool_name}"),
                "hint": "Use plugin_search to discover installed plugin tools."
            }));
        };
        if let Err(err) = Self::validate_plugin_arguments(&info, &arguments) {
            return Ok(err);
        }

        if let Some(wp) = self.wasm_plugins.iter().find(|p| p.name == plugin_name) {
            let notify_ctx = self.notification_tx.as_ref().map(|tx| {
                rsclaw_plugin::wasm_runtime::WasmNotifyCtx {
                    tx: tx.clone(),
                    target_id: if !ctx.chat_id.is_empty() {
                        ctx.chat_id.clone()
                    } else {
                        ctx.peer_id.clone()
                    },
                    channel: ctx.channel.clone(),
                }
            });
            return wp
                .call_tool_with_ctx(tool_name, arguments, notify_ctx)
                .await;
        }

        if let Some(reg) = self.plugins.as_ref()
            && let Some(plugin) = reg.get_js(plugin_name)
        {
            let target_id = if !ctx.chat_id.is_empty() {
                ctx.chat_id.clone()
            } else {
                ctx.peer_id.clone()
            };
            let params = serde_json::json!({
                "tool": tool_name,
                "args": arguments,
                "_ctx": {
                    "target_id": target_id,
                    "channel": ctx.channel.clone(),
                    "session_key": ctx.session_key.clone(),
                }
            });
            return plugin.call("tool_call", params).await;
        }

        Ok(json!({"error": format!("plugin runtime not loaded: {plugin_name}")}))
    }
}
