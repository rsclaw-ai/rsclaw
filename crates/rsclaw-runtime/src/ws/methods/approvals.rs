use crate::ws::dispatch::{MethodCtx, MethodResult};

pub async fn exec_approval_get(ctx: MethodCtx) -> MethodResult {
    let sandbox_mode = ctx
        .state
        .config
        .raw
        .sandbox
        .as_ref()
        .and_then(|s| s.mode.as_ref())
        .map(|m| format!("{m:?}").to_lowercase())
        .unwrap_or_else(|| "off".to_owned());

    Ok(serde_json::json!({
        "approvals": [],
        "strategy": {
            "security": {
                "mode": sandbox_mode,
            },
            "ask": { "mode": "auto" },
        },
    }))
}
pub async fn exec_approval_set(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({"ok": true}))
}
pub async fn exec_approval_resolve(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({"resolved": true, "ok": true}))
}

/// `exec.approvals.list` — return pending approvals (always empty for rsclaw).
pub async fn exec_approvals_list(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({ "approvals": [] }))
}

/// `exec.approvals.allowlist.get` — return the command allowlist.
pub async fn exec_approvals_allowlist_get(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({ "allowlist": [] }))
}

/// `exec.approvals.allowlist.set` — update the command allowlist (stub).
pub async fn exec_approvals_allowlist_set(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({ "ok": true }))
}
