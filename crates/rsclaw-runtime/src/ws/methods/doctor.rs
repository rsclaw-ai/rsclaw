use crate::ws::dispatch::{MethodCtx, MethodResult};

pub async fn doctor_run(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({"status": "ok", "issues": []}))
}
pub async fn doctor_memory_status(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({"status": "ok"}))
}
