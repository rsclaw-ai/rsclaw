use crate::ws::{
    dispatch::{MethodCtx, MethodResult},
    types::ErrorShape,
};

pub async fn config_set(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;
    let (path, mut val) = crate::cmd::config_json::load_config_json()
        .map_err(|e| ErrorShape::internal(e.to_string()))?;
    if let Some(key) = params.get("key").and_then(|v| v.as_str())
        && let Some(value) = params.get("value")
    {
        crate::cmd::config_json::set_nested_value(&mut val, key, value.clone())
            .map_err(|e| ErrorShape::internal(e.to_string()))?;
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&val).unwrap_or_default(),
    )
    .map_err(|e| ErrorShape::internal(e.to_string()))?;
    Ok(serde_json::json!({"ok": true}))
}
pub async fn config_patch(ctx: MethodCtx) -> MethodResult {
    config_set(ctx).await
}
pub async fn config_apply(_ctx: MethodCtx) -> MethodResult {
    match crate::config::load() {
        Ok(_) => Ok(serde_json::json!({"applied": true, "restarted": false})),
        Err(e) => Err(ErrorShape::internal(e.to_string())),
    }
}

/// config.schema — returns a schema descriptor for each config category.
///
/// The Control UI checks `result.available === true` and iterates
/// `result.fields` (flat array) to build tab-grouped forms.
/// We also emit a legacy `sections` map for older client versions.
pub async fn config_schema(ctx: MethodCtx) -> MethodResult {
    let params = ctx.req.params.as_ref();
    let category = params
        .and_then(|p| p.get("category"))
        .and_then(|v| v.as_str());

    // Load current config to show actual values alongside schema.
    let current = crate::cmd::config_json::load_config_json()
        .ok()
        .map(|(_, v)| v)
        .unwrap_or(serde_json::json!({}));

    let (tabs, fields) = match category {
        Some("settings") | None => build_settings_fields(&current),
        Some("communication") => build_communication_fields(&current),
        Some("automation") => build_automation_fields(&current),
        Some("infrastructure") => build_infrastructure_fields(&current),
        Some("ai") => build_ai_fields(&current),
        Some(cat) => {
            return Err(ErrorShape::bad_request(format!(
                "unknown schema category: {cat}"
            )));
        }
    };

    Ok(serde_json::json!({
        // Flag checked by the Control UI to decide whether to render forms.
        "available": true,
        "schema": "json5",
        "version": option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"),
        "category": category.unwrap_or("settings"),
        "categories": [
            { "id": "settings",       "label": "Settings" },
            { "id": "communication",  "label": "Communication" },
            { "id": "automation",     "label": "Automation" },
            { "id": "infrastructure", "label": "Infrastructure" },
            { "id": "ai",             "label": "AI & Agents" },
        ],
        // Flat list consumed by the form renderer (primary format).
        "tabs": tabs,
        "fields": fields,
    }))
}

// ---------------------------------------------------------------------------
// Field builders — each returns (tabs: Vec<String>, fields: Vec<Value>)
// ---------------------------------------------------------------------------

fn build_settings_fields(
    current: &serde_json::Value,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let tabs = tabs(&[
        "Settings",
        "Environment",
        "Authentication",
        "Updates",
        "Meta",
    ]);
    let fields = vec![
        f(
            "gateway.port",
            "number",
            "Gateway Port",
            "Settings",
            current.pointer("/gateway/port"),
            None,
        ),
        f(
            "gateway.mode",
            "select",
            "Gateway Mode",
            "Settings",
            current.pointer("/gateway/mode"),
            Some(&["Local", "Cloud"]),
        ),
        f(
            "gateway.bind",
            "select",
            "Bind Mode",
            "Settings",
            current.pointer("/gateway/bind"),
            Some(&["Loopback", "All"]),
        ),
        f(
            "gateway.reload",
            "select",
            "Reload Mode",
            "Settings",
            current.pointer("/gateway/reload"),
            Some(&["watch", "manual", "off"]),
        ),
        f(
            "gateway.auth.token",
            "secret",
            "Auth Token",
            "Settings",
            None,
            None,
        ),
        f(
            "gateway.controlUi.enabled",
            "boolean",
            "Control UI Enabled",
            "Settings",
            current.pointer("/gateway/controlUi/enabled"),
            None,
        ),
        f(
            "env",
            "object",
            "Environment Variables",
            "Environment",
            current.pointer("/env"),
            None,
        ),
        f(
            "auth.order",
            "array",
            "Auth Provider Order",
            "Authentication",
            current.pointer("/auth/order"),
            None,
        ),
        f(
            "update.channel",
            "select",
            "Update Channel",
            "Updates",
            current.pointer("/update/channel"),
            Some(&["stable", "beta", "nightly"]),
        ),
        f(
            "update.auto",
            "boolean",
            "Auto Update",
            "Updates",
            current.pointer("/update/auto"),
            None,
        ),
        f(
            "meta.version",
            "string",
            "Config Version",
            "Meta",
            current.pointer("/meta/version"),
            None,
        ),
        f(
            "meta.name",
            "string",
            "Instance Name",
            "Meta",
            current.pointer("/meta/name"),
            None,
        ),
    ];
    (tabs, fields)
}

fn build_communication_fields(
    current: &serde_json::Value,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let channel_names = [
        "telegram",
        "discord",
        "slack",
        "whatsapp",
        "signal",
        "feishu",
        "dingtalk",
        "wecom",
        "wechat",
        "mattermost",
        "msteams",
        "googlechat",
        "qq",
    ];

    let mut tab_names: Vec<&str> = channel_names.to_vec();
    tab_names.push("Session");
    let tabs = tabs(&tab_names);

    let mut fields = Vec::new();

    for name in &channel_names {
        let tab = *name;
        // Meta field: is this channel configured?
        let cfg = current.pointer(&format!("/channels/{name}"));
        let configured = cfg.is_some_and(|v| !v.is_null());
        fields.push(serde_json::json!({
            "key": format!("channels.{name}.__configured"),
            "type": "boolean",
            "label": "Enabled",
            "tab": tab,
            "value": configured,
            "readOnly": true,
        }));
        // Per-channel config fields.
        for cf in channel_fields(name, current) {
            let mut cf = cf;
            cf["tab"] = serde_json::json!(tab);
            fields.push(cf);
        }
    }

    fields.push(f(
        "session.dmScope",
        "select",
        "DM Scope",
        "Session",
        current.pointer("/session/dmScope"),
        Some(&["per_peer", "per_channel_peer", "main"]),
    ));

    (tabs, fields)
}

fn build_automation_fields(
    current: &serde_json::Value,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let tabs = tabs(&["Cron", "Hooks", "Approvals"]);
    let fields = vec![
        f(
            "cron.jobs",
            "array",
            "Scheduled Jobs",
            "Cron",
            current.pointer("/cron/jobs"),
            None,
        ),
        f(
            "cron.maxConcurrentRuns",
            "number",
            "Max Concurrent Runs",
            "Cron",
            current.pointer("/cron/maxConcurrentRuns"),
            None,
        ),
        f(
            "hooks",
            "object",
            "Webhook Handlers",
            "Hooks",
            current.pointer("/hooks"),
            None,
        ),
        f(
            "approvals",
            "object",
            "Approval Policies",
            "Approvals",
            current.pointer("/approvals"),
            None,
        ),
    ];
    (tabs, fields)
}

fn build_infrastructure_fields(
    current: &serde_json::Value,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let tabs = tabs(&["Sandbox", "Logging", "MCP"]);
    let fields = vec![
        f(
            "sandbox.enabled",
            "boolean",
            "Enable Sandbox",
            "Sandbox",
            current.pointer("/sandbox/enabled"),
            None,
        ),
        f(
            "sandbox.mode",
            "select",
            "Sandbox Mode",
            "Sandbox",
            current.pointer("/sandbox/mode"),
            Some(&["strict", "permissive"]),
        ),
        f(
            "logging.level",
            "select",
            "Log Level",
            "Logging",
            current.pointer("/logging/level"),
            Some(&["trace", "debug", "info", "warn", "error"]),
        ),
        f(
            "logging.format",
            "select",
            "Log Format",
            "Logging",
            current.pointer("/logging/format"),
            Some(&["text", "json"]),
        ),
        f(
            "logging.file",
            "string",
            "Log File Path",
            "Logging",
            current.pointer("/logging/file"),
            None,
        ),
        f(
            "mcp.servers",
            "object",
            "MCP Servers",
            "MCP",
            current.pointer("/mcp/servers"),
            None,
        ),
    ];
    (tabs, fields)
}

fn build_ai_fields(
    current: &serde_json::Value,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let tabs = tabs(&["Agents", "Models", "Tools", "Skills", "Memory"]);
    let fields = vec![
        f(
            "agents.defaults.model.primary",
            "string",
            "Default Model",
            "Agents",
            current.pointer("/agents/defaults/model/primary"),
            None,
        ),
        f(
            "agents.defaults.system",
            "text",
            "Default System Prompt",
            "Agents",
            current.pointer("/agents/defaults/system"),
            None,
        ),
        f(
            "agents.defaults.timeoutSeconds",
            "number",
            "Timeout (seconds)",
            "Agents",
            current.pointer("/agents/defaults/timeoutSeconds"),
            None,
        ),
        f(
            "agents.list",
            "array",
            "Agent List",
            "Agents",
            current.pointer("/agents/list"),
            None,
        ),
        f(
            "models",
            "object",
            "Provider Registry",
            "Models",
            current.pointer("/models"),
            None,
        ),
        f(
            "tools",
            "object",
            "Tool Configuration",
            "Tools",
            current.pointer("/tools"),
            None,
        ),
        f(
            "skills",
            "object",
            "Skill Configuration",
            "Skills",
            current.pointer("/skills"),
            None,
        ),
        f(
            "memory.enabled",
            "boolean",
            "Enable Memory",
            "Memory",
            current.pointer("/memory/enabled"),
            None,
        ),
        f(
            "memorySearch.provider",
            "string",
            "Embedding Provider",
            "Memory",
            current.pointer("/memorySearch/provider"),
            None,
        ),
    ];
    (tabs, fields)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a flat field descriptor with an optional options list.
fn f(
    key: &str,
    field_type: &str,
    label: &str,
    tab: &str,
    current: Option<&serde_json::Value>,
    options: Option<&[&str]>,
) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "key": key,
        "type": field_type,
        "label": label,
        "tab": tab,
        "value": current,
        "required": false,
    });
    if let Some(opts) = options {
        obj["options"] =
            serde_json::Value::Array(opts.iter().map(|o| serde_json::json!(o)).collect());
    }
    obj
}

/// Convert a slice of tab name strings into a JSON array of tab objects.
fn tabs(names: &[&str]) -> Vec<serde_json::Value> {
    names
        .iter()
        .map(|n| serde_json::json!({ "id": n.to_lowercase().replace(' ', "_"), "label": n }))
        .collect()
}

/// Per-channel field list with current values from config.
fn channel_fields(channel: &str, current: &serde_json::Value) -> Vec<serde_json::Value> {
    match channel {
        "telegram" => vec![
            f(
                "channels.telegram.token",
                "secret",
                "Bot Token",
                channel,
                None,
                None,
            ),
            f(
                "channels.telegram.dmPolicy",
                "select",
                "DM Policy",
                channel,
                current.pointer("/channels/telegram/dmPolicy"),
                Some(&["allow_all", "allow_known", "deny_all"]),
            ),
            f(
                "channels.telegram.draftChunk",
                "boolean",
                "Draft Chunk Mode",
                channel,
                current.pointer("/channels/telegram/draftChunk"),
                None,
            ),
        ],
        "discord" => vec![
            f(
                "channels.discord.token",
                "secret",
                "Bot Token",
                channel,
                None,
                None,
            ),
            f(
                "channels.discord.guildId",
                "string",
                "Guild ID",
                channel,
                current.pointer("/channels/discord/guildId"),
                None,
            ),
        ],
        "slack" => vec![
            f(
                "channels.slack.botToken",
                "secret",
                "Bot Token",
                channel,
                None,
                None,
            ),
            f(
                "channels.slack.appToken",
                "secret",
                "App Token",
                channel,
                None,
                None,
            ),
        ],
        "whatsapp" => vec![
            f(
                "channels.whatsapp.apiUrl",
                "string",
                "API URL",
                channel,
                current.pointer("/channels/whatsapp/apiUrl"),
                None,
            ),
            f(
                "channels.whatsapp.token",
                "secret",
                "Token",
                channel,
                None,
                None,
            ),
        ],
        "signal" => vec![
            f(
                "channels.signal.apiUrl",
                "string",
                "API URL",
                channel,
                current.pointer("/channels/signal/apiUrl"),
                None,
            ),
            f(
                "channels.signal.phoneNumber",
                "string",
                "Phone Number",
                channel,
                current.pointer("/channels/signal/phoneNumber"),
                None,
            ),
        ],
        "feishu" => vec![
            f(
                "channels.feishu.appId",
                "string",
                "App ID",
                channel,
                current.pointer("/channels/feishu/appId"),
                None,
            ),
            f(
                "channels.feishu.appSecret",
                "secret",
                "App Secret",
                channel,
                None,
                None,
            ),
        ],
        "dingtalk" => vec![
            f(
                "channels.dingtalk.appKey",
                "string",
                "App Key",
                channel,
                current.pointer("/channels/dingtalk/appKey"),
                None,
            ),
            f(
                "channels.dingtalk.appSecret",
                "secret",
                "App Secret",
                channel,
                None,
                None,
            ),
        ],
        "wecom" => vec![
            f(
                "channels.wecom.corpId",
                "string",
                "Corp ID",
                channel,
                current.pointer("/channels/wecom/corpId"),
                None,
            ),
            f(
                "channels.wecom.agentId",
                "string",
                "Agent ID",
                channel,
                current.pointer("/channels/wecom/agentId"),
                None,
            ),
            f(
                "channels.wecom.secret",
                "secret",
                "Secret",
                channel,
                None,
                None,
            ),
        ],
        "wechat" => vec![
            f(
                "channels.wechat.appId",
                "string",
                "App ID",
                channel,
                current.pointer("/channels/wechat/appId"),
                None,
            ),
            f(
                "channels.wechat.appSecret",
                "secret",
                "App Secret",
                channel,
                None,
                None,
            ),
        ],
        "mattermost" => vec![
            f(
                "channels.mattermost.url",
                "string",
                "Server URL",
                channel,
                current.pointer("/channels/mattermost/url"),
                None,
            ),
            f(
                "channels.mattermost.token",
                "secret",
                "Bot Token",
                channel,
                None,
                None,
            ),
        ],
        "qq" => vec![
            f(
                "channels.qq.appId",
                "string",
                "App ID",
                channel,
                current.pointer("/channels/qq/appId"),
                None,
            ),
            f(
                "channels.qq.appSecret",
                "secret",
                "App Secret",
                channel,
                None,
                None,
            ),
            f(
                "channels.qq.sandbox",
                "boolean",
                "Sandbox Mode",
                channel,
                current.pointer("/channels/qq/sandbox"),
                None,
            ),
        ],
        _ => vec![f(
            &format!("channels.{channel}"),
            "object",
            "Configuration",
            channel,
            current.pointer(&format!("/channels/{channel}")),
            None,
        )],
    }
}
