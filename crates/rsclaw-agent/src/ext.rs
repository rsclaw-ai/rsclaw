//! Native agent tools registered by an extension crate.
//!
//! A custom engine binary links an extension crate and, before starting the
//! gateway, calls [`register_tool_provider`]. The provider's tools then join
//! the tool list of every agent that admits them and are dispatched to the
//! provider. The stock binary registers nothing, so its tool lists and
//! dispatch are unchanged.
//!
//! Rules:
//! - Every tool a provider offers is named `<namespace>_<tool>`.
//! - An agent sees a provider's tools only when its toolset is `full` with no
//!   `tools` list, or its `tools` list names them exactly or with
//!   `"<namespace>_*"`. The `minimal` / `web` / `code` / `standard` presets
//!   never include them.
//! - A tool with a `group` stays behind the `request_tool` stub until the model
//!   enables `ext:<namespace>:<group>` for the session, like plugin tool
//!   groups.
//! - Dispatch applies the usual whitelist, owner-only and `approvals.tools`
//!   gates; [`ExtTool::owner_only`] marks a tool owner-only like the built-in
//!   host tools.

use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, RwLock},
};

pub use futures::future::BoxFuture;
pub use rsclaw_provider::ToolDef;
use serde_json::Value;

/// One tool an extension offers an agent.
#[derive(Debug, Clone)]
pub struct ExtTool {
    /// Name, description and JSON schema shown to the model. The name must
    /// be `<namespace>_<tool>` (ASCII letters, digits and single `_`);
    /// tools with any other name are dropped with a warning.
    pub def: ToolDef,
    /// Tool group. A grouped tool is offered behind the `request_tool` stub
    /// as `ext:<namespace>:<group>` and joins the live list once the model
    /// enables that group for the session.
    pub group: Option<String>,
    /// Refuse calls from non-owner senders unless the agent lists the tool
    /// in `nonOwnerTools`, like the built-in host tools.
    pub owner_only: bool,
}

/// The turn an extension tool call belongs to.
#[derive(Debug, Clone)]
pub struct ExtToolCtx {
    pub agent_id: String,
    pub session_key: String,
    pub channel: String,
    pub peer_id: String,
    /// A person started this turn (not a heartbeat, system tick or a job's
    /// completion notice). Same notion as the `approvals.tools` gate.
    pub person_started: bool,
}

/// Source of native agent tools under one namespace.
///
/// `call` returns a boxed future so the trait stays object-safe; implement
/// it as `Box::pin(async move { ... })`.
pub trait ToolProvider: Send + Sync {
    /// Namespace: 1-32 lowercase ASCII letters and digits, starting with a
    /// letter. Every tool name starts with `<namespace>_`.
    fn namespace(&self) -> &str;

    /// Tools offered to `agent_id`. Called when a turn builds its tool list
    /// and when one of the tools is called, so it should be cheap.
    fn tools(&self, agent_id: &str) -> Vec<ExtTool>;

    /// Run tool `name` (full name, namespace included). The result goes
    /// back to the model as the tool result; an `Err` is reported to the
    /// model as the tool's error.
    fn call<'a>(
        &'a self,
        ctx: ExtToolCtx,
        name: &'a str,
        args: Value,
    ) -> BoxFuture<'a, anyhow::Result<Value>>;
}

type Registered = (String, Arc<dyn ToolProvider>);

static PROVIDERS: RwLock<Vec<Registered>> = RwLock::new(Vec::new());

/// Register a tool provider. Registering the same namespace again replaces
/// the earlier provider.
///
/// # Panics
/// If the namespace is malformed or reserved (`agent`, `mcp`, `ext`), or a
/// built-in tool name starts with `<namespace>_`.
pub fn register_tool_provider(provider: Arc<dyn ToolProvider>) {
    let namespace = provider.namespace().to_owned();
    if let Err(why) = check_namespace(&namespace) {
        panic!("tool provider rejected: {why}");
    }
    let mut providers = PROVIDERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    providers.retain(|(ns, _)| *ns != namespace);
    providers.push((namespace, provider));
}

/// Namespaces an extension may not claim: `agent_<id>` and `mcp_<server>_`
/// are dispatch prefixes, `ext` is the group key prefix.
const RESERVED_NAMESPACES: &[&str] = &["agent", "mcp", "ext"];

/// Built-in names dispatch accepts beyond [`rsclaw_types::BUILTIN_TOOL_NAMES`]
/// and [`crate::trust::OWNER_ONLY_TOOLS`]: legacy aliases, channel action
/// tools and the stock plugin aliases. A namespace may not prefix any of
/// them.
const BUILTIN_ALIASES: &[&str] = &[
    "memory_search",
    "memory_get",
    "memory_put",
    "memory_delete",
    "session_status",
    "agents_list",
    "agent_list",
    "kb_search",
    "stock_quote",
    "stock_kline",
    "stock_snapshot",
    "stock_ask",
    "stock_query",
    "stock_chart",
    "stock_watchlist",
    "research_ingest_wechat",
    "research_analyze_charts",
    "plugin_info",
    "plugin_search_tools",
    "plugin_describe_tool",
    "video_status",
    "text_to_speech",
    "request_tool",
    "wait_auth",
];

/// Why `namespace` cannot be registered, if it cannot.
pub(crate) fn check_namespace(namespace: &str) -> Result<(), String> {
    let well_formed = namespace.len() <= 32
        && namespace
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase())
        && namespace
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !well_formed {
        return Err(format!(
            "namespace {namespace:?} must be 1-32 lowercase ASCII letters or digits, \
             starting with a letter"
        ));
    }
    if RESERVED_NAMESPACES.contains(&namespace) {
        return Err(format!("namespace {namespace:?} is reserved"));
    }
    let prefix = format!("{namespace}_");
    if let Some(clash) = rsclaw_types::BUILTIN_TOOL_NAMES
        .iter()
        .chain(crate::trust::OWNER_ONLY_TOOLS)
        .chain(crate::tools_builder::COLD_TOOLS)
        .chain(BUILTIN_ALIASES)
        .find(|name| name.starts_with(&prefix))
    {
        return Err(format!(
            "namespace {namespace:?} collides with built-in tool {clash:?}"
        ));
    }
    Ok(())
}

/// `name` is `<namespace>_<tool>` with a well-formed tool part.
fn tool_name_ok(namespace: &str, name: &str) -> bool {
    name.len() <= 64
        && name
            .strip_prefix(namespace)
            .and_then(|rest| rest.strip_prefix('_'))
            .is_some_and(|tool| {
                !tool.is_empty()
                    && !tool.starts_with('_')
                    && !tool.contains("__")
                    && tool.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
}

fn group_ok(group: &str) -> bool {
    !group.is_empty()
        && group
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn snapshot() -> Vec<Registered> {
    PROVIDERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// True when at least one provider is registered.
pub(crate) fn any_registered() -> bool {
    !PROVIDERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_empty()
}

/// The tools `provider` offers `agent_id`, minus malformed ones.
fn valid_tools(namespace: &str, provider: &dyn ToolProvider, agent_id: &str) -> Vec<ExtTool> {
    let mut tools = provider.tools(agent_id);
    tools.retain(|t| {
        let ok = tool_name_ok(namespace, &t.def.name) && t.group.as_deref().is_none_or(group_ok);
        if !ok {
            tracing::warn!(
                namespace,
                tool = %t.def.name,
                group = ?t.group,
                "extension tool dropped: name must be `<namespace>_<tool>`, group alphanumeric"
            );
        }
        ok
    });
    tools
}

/// Whether an agent's allowlist (`None` = unrestricted) admits extension
/// tool `name` of `namespace`: it must name the tool or `"<namespace>_*"`.
pub(crate) fn admitted(allowed: Option<&HashSet<String>>, namespace: &str, name: &str) -> bool {
    allowed.is_none_or(|set| set.contains(name) || set.contains(&format!("{namespace}_*")))
}

/// The registered namespace that owns `name`. Plugin (`<plugin>__<tool>`)
/// and skill (`<skill>.<tool>`) names are never extension names, so a
/// plugin whose name starts with a namespace keeps its tools.
fn owner<'a>(providers: &'a [Registered], name: &str) -> Option<&'a Registered> {
    if name.contains("__") || name.contains('.') {
        return None;
    }
    providers.iter().find(|(ns, _)| {
        name.strip_prefix(ns.as_str())
            .is_some_and(|rest| rest.starts_with('_'))
    })
}

/// Whether `allowed` admits `name` through a `"<namespace>_*"` entry of a
/// registered namespace. Exact names are the caller's own check.
pub(crate) fn pattern_admits(allowed: &HashSet<String>, name: &str) -> bool {
    if !any_registered() {
        return false;
    }
    owner(&snapshot(), name).is_some_and(|(ns, _)| allowed.contains(&format!("{ns}_*")))
}

/// Extension tools for one agent's turn.
#[derive(Debug, Default)]
pub(crate) struct ExtToolList {
    /// Definitions to offer the model now.
    pub live: Vec<ToolDef>,
    /// Grouped tools behind the stub, keyed `ext:<namespace>:<group>`.
    pub deferred: Vec<(String, ToolDef)>,
    /// `request_tool` stub lines: (enable key, label).
    pub stub_entries: Vec<(String, String)>,
}

/// Extension tools for `agent_id` under allowlist `allowed` (`None` =
/// unrestricted); `enabled` holds the session's `request_tool` enables.
pub(crate) fn tool_list(
    agent_id: &str,
    allowed: Option<&HashSet<String>>,
    enabled: &HashSet<String>,
) -> ExtToolList {
    tool_list_from(&snapshot(), agent_id, allowed, enabled)
}

fn tool_list_from(
    providers: &[Registered],
    agent_id: &str,
    allowed: Option<&HashSet<String>>,
    enabled: &HashSet<String>,
) -> ExtToolList {
    let mut out = ExtToolList::default();
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (ns, provider) in providers {
        for tool in valid_tools(ns, provider.as_ref(), agent_id) {
            if !admitted(allowed, ns, &tool.def.name) {
                continue;
            }
            match tool.group {
                None => out.live.push(tool.def),
                Some(group) => {
                    let key = format!("ext:{ns}:{group}");
                    if enabled.contains(&key) {
                        out.live.push(tool.def);
                    } else {
                        groups
                            .entry(key.clone())
                            .or_default()
                            .push(tool.def.name.clone());
                        out.deferred.push((key, tool.def));
                    }
                }
            }
        }
    }
    out.stub_entries = groups
        .into_iter()
        .map(|(key, names)| {
            let label = format!("{key} ({})", names.join(", "));
            (key, label)
        })
        .collect();
    out
}

/// Whether `key` (`ext:<namespace>:<group>`) is a tool group offered to
/// `agent_id` and admitted by `allowed`.
pub(crate) fn is_group(agent_id: &str, allowed: Option<&HashSet<String>>, key: &str) -> bool {
    is_group_in(&snapshot(), agent_id, allowed, key)
}

fn is_group_in(
    providers: &[Registered],
    agent_id: &str,
    allowed: Option<&HashSet<String>>,
    key: &str,
) -> bool {
    let Some((ns, group)) = key.strip_prefix("ext:").and_then(|k| k.split_once(':')) else {
        return false;
    };
    providers
        .iter()
        .filter(|(n, _)| n == ns)
        .any(|(n, provider)| {
            valid_tools(n, provider.as_ref(), agent_id)
                .iter()
                .any(|t| t.group.as_deref() == Some(group) && admitted(allowed, n, &t.def.name))
        })
}

/// An extension tool resolved for one agent.
pub(crate) struct Resolved {
    pub namespace: String,
    pub provider: Arc<dyn ToolProvider>,
    pub tool: ExtTool,
}

/// The extension tool `name` for `agent_id`. `Ok(None)` when no registered
/// namespace owns the name; `Err` when one does but offers no such tool.
pub(crate) fn resolve(agent_id: &str, name: &str) -> anyhow::Result<Option<Resolved>> {
    if !any_registered() {
        return Ok(None);
    }
    resolve_in(&snapshot(), agent_id, name)
}

fn resolve_in(
    providers: &[Registered],
    agent_id: &str,
    name: &str,
) -> anyhow::Result<Option<Resolved>> {
    let Some((ns, provider)) = owner(providers, name) else {
        return Ok(None);
    };
    match valid_tools(ns, provider.as_ref(), agent_id)
        .into_iter()
        .find(|t| t.def.name == name)
    {
        Some(tool) => Ok(Some(Resolved {
            namespace: ns.clone(),
            provider: Arc::clone(provider),
            tool,
        })),
        None => Err(anyhow::anyhow!(
            "unknown tool: `{name}` (the `{ns}` tools offered to this agent do not include it)"
        )),
    }
}

/// Call a resolved tool, refusing it when the agent's allowlist (`None` =
/// unrestricted) does not admit it.
pub(crate) async fn call(
    resolved: Resolved,
    allowed: Option<&HashSet<String>>,
    ctx: ExtToolCtx,
    name: &str,
    args: Value,
) -> anyhow::Result<Value> {
    if !admitted(allowed, &resolved.namespace, name) {
        return Err(anyhow::anyhow!(
            "tool '{name}' is not enabled for this agent. Do not retry it; answer \
             with the tools that are available."
        ));
    }
    resolved.provider.call(ctx, name, args).await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    struct Fake {
        ns: String,
        tools: Vec<ExtTool>,
        seen: Mutex<Vec<(ExtToolCtx, String, Value)>>,
    }

    impl ToolProvider for Fake {
        fn namespace(&self) -> &str {
            &self.ns
        }
        fn tools(&self, _agent_id: &str) -> Vec<ExtTool> {
            self.tools.clone()
        }
        fn call<'a>(
            &'a self,
            ctx: ExtToolCtx,
            name: &'a str,
            args: Value,
        ) -> BoxFuture<'a, anyhow::Result<Value>> {
            Box::pin(async move {
                self.seen
                    .lock()
                    .expect("test lock")
                    .push((ctx, name.to_owned(), args.clone()));
                Ok(json!({"ok": name, "args": args}))
            })
        }
    }

    fn tool(name: &str, group: Option<&str>, owner_only: bool) -> ExtTool {
        ExtTool {
            def: ToolDef {
                name: name.to_owned(),
                description: format!("{name} tool"),
                parameters: json!({"type": "object", "properties": {}}),
            },
            group: group.map(str::to_owned),
            owner_only,
        }
    }

    fn fake(ns: &str, tools: Vec<ExtTool>) -> Arc<Fake> {
        Arc::new(Fake {
            ns: ns.to_owned(),
            tools,
            seen: Mutex::new(Vec::new()),
        })
    }

    fn registered(p: &Arc<Fake>) -> Vec<Registered> {
        vec![(p.ns.clone(), p.clone() as Arc<dyn ToolProvider>)]
    }

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn names(defs: &[ToolDef]) -> Vec<&str> {
        defs.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn ext_tool_namespace_validation() {
        assert!(check_namespace("shop2").is_ok());
        assert!(check_namespace("crm").is_ok());
        for bad in ["", "Shop", "2shop", "my_ns", "my-ns", &"a".repeat(33)] {
            assert!(check_namespace(bad).is_err(), "{bad:?} should be malformed");
        }
        for reserved in ["agent", "mcp", "ext"] {
            assert!(check_namespace(reserved).is_err(), "{reserved} is reserved");
        }
        // Built-in names under these prefixes would be shadowed.
        for clash in [
            "web", "memory", "skill", "plugin", "cap", "stock", "create", "sessions",
        ] {
            let err = check_namespace(clash).expect_err(clash);
            assert!(err.contains("collides"), "{clash}: {err}");
        }
    }

    #[test]
    #[should_panic(expected = "tool provider rejected")]
    fn ext_tool_register_rejects_builtin_collision() {
        register_tool_provider(fake("web", vec![tool("web_thing", None, false)]));
    }

    #[test]
    fn ext_tool_register_same_namespace_replaces() {
        register_tool_provider(fake("extreplace", vec![tool("extreplace_a", None, false)]));
        register_tool_provider(fake("extreplace", vec![tool("extreplace_b", None, false)]));
        let list = tool_list("main", None, &HashSet::new());
        let live = names(&list.live);
        assert!(live.contains(&"extreplace_b"));
        assert!(!live.contains(&"extreplace_a"));
        assert_eq!(
            snapshot()
                .iter()
                .filter(|(ns, _)| ns == "extreplace")
                .count(),
            1
        );
    }

    #[test]
    fn ext_tool_off_namespace_names_are_dropped() {
        let p = fake(
            "shop",
            vec![
                tool("shop_ok", None, false),
                tool("other_tool", None, false),
                tool("shop_", None, false),
                tool("shop__x", None, false),
                tool("shopx", None, false),
                tool("shop_bad", Some("a:b"), false),
            ],
        );
        let list = tool_list_from(&registered(&p), "main", None, &HashSet::new());
        assert_eq!(names(&list.live), vec!["shop_ok"]);
        assert!(list.deferred.is_empty());
    }

    #[test]
    fn ext_tool_list_follows_allowlist_and_pattern() {
        let p = fake(
            "shop",
            vec![
                tool("shop_search", None, false),
                tool("shop_order", None, false),
            ],
        );
        let reg = registered(&p);
        let none = HashSet::new();
        // `full` with no tools list: everything.
        let all = tool_list_from(&reg, "main", None, &none);
        assert_eq!(names(&all.live), vec!["shop_search", "shop_order"]);
        // A preset allowlist does not name them: nothing.
        let preset = set(&["shell", "read_file", "web_search"]);
        assert!(
            tool_list_from(&reg, "main", Some(&preset), &none)
                .live
                .is_empty()
        );
        // Exact name.
        let exact = set(&["shell", "shop_order"]);
        assert_eq!(
            names(&tool_list_from(&reg, "main", Some(&exact), &none).live),
            vec!["shop_order"]
        );
        // Namespace pattern.
        let pattern = set(&["shop_*"]);
        assert_eq!(
            names(&tool_list_from(&reg, "main", Some(&pattern), &none).live),
            vec!["shop_search", "shop_order"]
        );
        // Another namespace's pattern does not match.
        let other = set(&["shopx_*"]);
        assert!(
            tool_list_from(&reg, "main", Some(&other), &none)
                .live
                .is_empty()
        );
    }

    #[test]
    fn ext_tool_groups_stay_behind_request_tool_until_enabled() {
        let p = fake(
            "shop",
            vec![
                tool("shop_search", None, false),
                tool("shop_refund", Some("orders"), false),
                tool("shop_cancel", Some("orders"), false),
            ],
        );
        let reg = registered(&p);
        let cold = tool_list_from(&reg, "main", None, &HashSet::new());
        assert_eq!(names(&cold.live), vec!["shop_search"]);
        assert_eq!(cold.deferred.len(), 2);
        assert!(cold.deferred.iter().all(|(k, _)| k == "ext:shop:orders"));
        assert_eq!(
            cold.stub_entries,
            vec![(
                "ext:shop:orders".to_owned(),
                "ext:shop:orders (shop_refund, shop_cancel)".to_owned()
            )]
        );
        assert!(is_group_in(&reg, "main", None, "ext:shop:orders"));
        assert!(!is_group_in(&reg, "main", None, "ext:shop:nope"));
        assert!(!is_group_in(&reg, "main", None, "shop:orders"));
        // An allowlist that does not admit the tools hides the group too.
        assert!(!is_group_in(
            &reg,
            "main",
            Some(&set(&["shell"])),
            "ext:shop:orders"
        ));

        let warm = tool_list_from(&reg, "main", None, &set(&["ext:shop:orders"]));
        assert_eq!(
            names(&warm.live),
            vec!["shop_search", "shop_refund", "shop_cancel"]
        );
        assert!(warm.deferred.is_empty());
        assert!(warm.stub_entries.is_empty());
    }

    #[tokio::test]
    async fn ext_tool_dispatch_reaches_provider_with_ctx() {
        let p = fake("shop", vec![tool("shop_search", None, false)]);
        let reg = registered(&p);
        let resolved = resolve_in(&reg, "main", "shop_search")
            .expect("resolves")
            .expect("owned by shop");
        let ctx = ExtToolCtx {
            agent_id: "main".to_owned(),
            session_key: "agent:main:feishu:direct:u1".to_owned(),
            channel: "feishu".to_owned(),
            peer_id: "u1".to_owned(),
            person_started: true,
        };
        let out = call(
            resolved,
            Some(&set(&["shop_*"])),
            ctx,
            "shop_search",
            json!({"q": "tea"}),
        )
        .await
        .expect("call succeeds");
        assert_eq!(out["ok"], "shop_search");
        let seen = p.seen.lock().expect("test lock");
        assert_eq!(seen.len(), 1);
        let (ctx, name, args) = &seen[0];
        assert_eq!(name, "shop_search");
        assert_eq!(args["q"], "tea");
        assert_eq!(ctx.agent_id, "main");
        assert_eq!(ctx.session_key, "agent:main:feishu:direct:u1");
        assert_eq!(ctx.channel, "feishu");
        assert_eq!(ctx.peer_id, "u1");
        assert!(ctx.person_started);
    }

    #[tokio::test]
    async fn ext_tool_dispatch_refuses_unadmitted() {
        let p = fake("shop", vec![tool("shop_search", None, false)]);
        let resolved = resolve_in(&registered(&p), "main", "shop_search")
            .expect("resolves")
            .expect("owned by shop");
        let ctx = ExtToolCtx {
            agent_id: "main".to_owned(),
            session_key: "s".to_owned(),
            channel: "ws".to_owned(),
            peer_id: String::new(),
            person_started: true,
        };
        let err = call(
            resolved,
            Some(&set(&["shell"])),
            ctx,
            "shop_search",
            json!({}),
        )
        .await
        .expect_err("not admitted");
        assert!(err.to_string().contains("not enabled"));
        assert!(p.seen.lock().expect("test lock").is_empty());
    }

    #[test]
    fn ext_tool_unknown_name_errors_cleanly() {
        let p = fake("shop", vec![tool("shop_search", None, false)]);
        let reg = registered(&p);
        let err = resolve_in(&reg, "main", "shop_missing")
            .err()
            .expect("unknown ext tool is an error");
        assert!(err.to_string().contains("unknown tool: `shop_missing`"));
        // Names outside every namespace are not ours.
        assert!(
            resolve_in(&reg, "main", "web_search")
                .expect("ok")
                .is_none()
        );
        assert!(
            resolve_in(&reg, "main", "shopping_list")
                .expect("ok")
                .is_none()
        );
        // A plugin whose name starts with the namespace keeps its tools.
        assert!(
            resolve_in(&reg, "main", "shop_helper__sync")
                .expect("ok")
                .is_none()
        );
        assert!(
            resolve_in(&reg, "main", "shop_skill.run")
                .expect("ok")
                .is_none()
        );
    }

    #[test]
    fn ext_tool_owner_only_refused_for_non_owner() {
        use crate::trust::{SenderTrust, owner_only_tool_allowed};
        let p = fake(
            "shop",
            vec![
                tool("shop_refund", None, true),
                tool("shop_search", None, false),
            ],
        );
        let reg = registered(&p);
        let refund = resolve_in(&reg, "main", "shop_refund")
            .expect("ok")
            .expect("found");
        let search = resolve_in(&reg, "main", "shop_search")
            .expect("ok")
            .expect("found");
        assert!(refund.tool.owner_only);
        // Same decision dispatch_tool makes for an extension tool.
        fn gate(r: &Resolved, trust: SenderTrust, non_owner: Option<&[String]>) -> bool {
            owner_only_tool_allowed(
                trust,
                &r.tool.def.name,
                crate::trust::is_owner_only_tool(&r.tool.def.name) || r.tool.owner_only,
                non_owner,
            )
        }
        assert!(!gate(&refund, SenderTrust::User, None));
        assert!(gate(&refund, SenderTrust::Owner, None));
        assert!(gate(&search, SenderTrust::User, None));
        let allow = vec!["shop_refund".to_owned()];
        assert!(gate(&refund, SenderTrust::User, Some(allow.as_slice())));
    }

    #[test]
    fn ext_tool_pattern_admits_only_registered_namespaces() {
        register_tool_provider(fake("extpattern", vec![tool("extpattern_a", None, false)]));
        let allowed = set(&["extpattern_*", "web_*"]);
        assert!(pattern_admits(&allowed, "extpattern_a"));
        assert!(!pattern_admits(&allowed, "web_search"));
        assert!(!pattern_admits(&set(&["shell"]), "extpattern_a"));
    }
}
