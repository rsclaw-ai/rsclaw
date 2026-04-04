use crate::acp::types::{PermissionOption, RequestPermissionOutcome};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct PendingPermission {
    pub tool_call_id: String,
    pub session_id: String,
    pub options: Vec<PermissionOption>,
    pub response_tx: Arc<tokio::sync::oneshot::Sender<RequestPermissionOutcome>>,
}

pub type PermissionMap = Arc<Mutex<HashMap<String, PendingPermission>>>;

static PERMISSION_CONTEXT: std::sync::OnceLock<PermissionMap> = std::sync::OnceLock::new();

pub fn permission_context() -> &'static PermissionMap {
    PERMISSION_CONTEXT.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

pub async fn add_pending_permission(chat_id: String, permission: PendingPermission) {
    let mut ctx = permission_context().lock().await;
    ctx.insert(chat_id, permission);
}

pub async fn remove_pending_permission(chat_id: &str) -> Option<PendingPermission> {
    let mut ctx = permission_context().lock().await;
    ctx.remove(chat_id)
}

pub async fn get_pending_permission(chat_id: &str) -> Option<PendingPermission> {
    let ctx = permission_context().lock().await;
    ctx.get(chat_id).cloned()
}

pub async fn resolve_permission(chat_id: &str, outcome: RequestPermissionOutcome) -> bool {
    if let Some(pending) = remove_pending_permission(chat_id).await {
        if let Ok(sender) = std::sync::Arc::try_unwrap(pending.response_tx) {
            let _ = sender.send(outcome);
            true
        } else {
            false
        }
    } else {
        false
    }
}
