use crate::ws::{
    dispatch::{MethodCtx, MethodResult},
    types::ErrorShape,
};

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
/// `exec.approval.set` — not implemented (rsclaw has no exec-approval queue).
pub async fn exec_approval_set(_ctx: MethodCtx) -> MethodResult {
    Err(ErrorShape::not_implemented(
        "exec.approval.set is not implemented; configure sandbox/exec policy in the config file",
    ))
}
/// `exec.approval.resolve` — not implemented (no pending approvals exist).
pub async fn exec_approval_resolve(_ctx: MethodCtx) -> MethodResult {
    Err(ErrorShape::not_implemented(
        "exec.approval.resolve is not implemented; rsclaw has no pending exec approvals",
    ))
}

/// `exec.approvals.list` — return pending approvals (always empty for rsclaw).
pub async fn exec_approvals_list(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({ "approvals": [] }))
}

/// `exec.approvals.allowlist.get` — return the command allowlist.
pub async fn exec_approvals_allowlist_get(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({ "allowlist": [] }))
}

/// `exec.approvals.allowlist.set` — not implemented; reporting success
/// would make clients believe the allowlist changed.
pub async fn exec_approvals_allowlist_set(_ctx: MethodCtx) -> MethodResult {
    Err(ErrorShape::not_implemented(
        "exec.approvals.allowlist.set is not implemented; edit the exec allowlist in the config file",
    ))
}
