//! Method routing — dispatches `ReqFrame` to the correct handler.

use std::sync::Arc;

use tokio::sync::RwLock;

use super::{
    conn::ConnHandle,
    methods,
    types::{ErrorShape, ReqFrame},
};
use crate::server::AppState;

pub type MethodResult = Result<serde_json::Value, ErrorShape>;

pub struct MethodCtx {
    pub req: ReqFrame,
    pub state: AppState,
    pub conn: Arc<RwLock<ConnHandle>>,
}

pub async fn dispatch(ctx: MethodCtx) -> MethodResult {
    tracing::info!(method = %ctx.req.method, "ws dispatch");
    match ctx.req.method.as_str() {
        // sessions
        "sessions.list" => methods::sessions::sessions_list(ctx).await,
        "sessions.send" => methods::sessions::sessions_send(ctx).await,
        "sessions.messages.subscribe" => methods::sessions::sessions_messages_subscribe(ctx).await,
        "sessions.messages.unsubscribe" => {
            methods::sessions::sessions_messages_unsubscribe(ctx).await
        }
        "sessions.reset" => methods::sessions::sessions_reset(ctx).await,
        "sessions.delete" => methods::sessions::sessions_delete(ctx).await,

        // chat
        "chat.send" => methods::chat::chat_send(ctx).await,
        "chat.history" => methods::chat::chat_history(ctx).await,
        "chat.abort" => methods::chat::chat_abort(ctx).await,
        "chat.inject" => methods::chat::chat_inject(ctx).await,

        // agents
        "agents.list" => methods::agents::agents_list(ctx).await,
        "agents.create" => methods::agents::agents_create(ctx).await,
        "agents.update" => methods::agents::agents_update(ctx).await,
        "agents.delete" => methods::agents::agents_delete(ctx).await,

        // system
        "health" => methods::system::health(ctx).await,
        "status" => methods::system::status(ctx).await,
        "models.list" => methods::system::models_list(ctx).await,
        "config.get" => methods::system::config_get(ctx).await,

        // cron
        "cron.list" => methods::system::cron_list(ctx).await,
        "cron.add" => methods::system::cron_add(ctx).await,
        "cron.remove" => methods::system::cron_remove(ctx).await,
        "cron.run" => methods::system::cron_run(ctx).await,
        "cron.update" => methods::system::cron_update(ctx).await,
        "cron.delete" => methods::system::cron_delete(ctx).await,

        // extensions
        "memory.search" => methods::extensions::memory_search(ctx).await,
        "memory.store" => methods::extensions::memory_store(ctx).await,
        "memory.status" => methods::extensions::memory_status(ctx).await,
        "plugins.list" => methods::extensions::plugins_list(ctx).await,
        "hooks.list" => methods::extensions::hooks_list(ctx).await,

        // agent identity + send
        "agent.identity.get" => methods::agents::agent_identity_get(ctx).await,
        "agent.send" => methods::sessions::sessions_send(ctx).await,

        // skills
        "skills.status" | "skills.list" => {
            let global_dir = crate::skill::default_global_skills_dir().unwrap_or_default();
            let registry =
                crate::skill::load_skills(&global_dir, None, ctx.state.config.ext.skills.as_ref())
                    .unwrap_or_default();
            let skills: Vec<serde_json::Value> = registry
                .all()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "description": s.description.as_deref().unwrap_or(""),
                        "version": s.version.as_deref().unwrap_or(""),
                        "author": s.extra.get("author").and_then(|v| v.as_str()).unwrap_or(""),
                        "icon": s.extra.get("icon").and_then(|v| v.as_str()).unwrap_or(""),
                        "tools": s.tools.iter().map(|t| serde_json::json!({"name": t.name, "description": t.description})).collect::<Vec<_>>(),
                        "source": crate::config::loader::path_to_forward_slash(&s.dir),
                        "filePath": crate::config::loader::path_to_forward_slash(&s.dir.join("SKILL.md")),
                        "baseDir": crate::config::loader::path_to_forward_slash(&s.dir),
                        "path": crate::config::loader::path_to_forward_slash(&s.dir),
                        "skillKey": s.name,
                        "bundled": false,
                        "always": false,
                        "disabled": false,
                        "blockedByAllowlist": false,
                        "eligible": true,
                        "requirements": { "bins": [], "env": [], "config": [], "os": [] },
                        "missing": { "bins": [], "env": [], "config": [], "os": [] },
                        "configChecks": [],
                        "install": [],
                    })
                })
                .collect();
            Ok(serde_json::json!({
                "workspaceDir": crate::config::loader::path_to_forward_slash(&global_dir),
                "managedSkillsDir": crate::config::loader::path_to_forward_slash(&global_dir),
                "skills": skills,
            }))
        }

        // node management (return empty — rsclaw doesn't have hardware nodes)
        "node.list" => Ok(serde_json::json!({"nodes": []})),
        "node.pair.list" => Ok(serde_json::json!({"requests": []})),
        "device.pair.list" | "device.list" => Ok(serde_json::json!({"devices": []})),
        "skills.bins" => Ok(serde_json::json!({"bins": []})),

        // stubs for WebUI polling methods
        "exec.approval.list" | "exec.approvals.list" => {
            methods::approvals::exec_approvals_list(ctx).await
        }
        "exec.approvals.allowlist.get" => {
            methods::approvals::exec_approvals_allowlist_get(ctx).await
        }
        "exec.approvals.allowlist.set" => {
            methods::approvals::exec_approvals_allowlist_set(ctx).await
        }
        "tools.catalog" => methods::catalog::tools_catalog(ctx).await,
        "tools.effective" => methods::catalog::tools_effective(ctx).await,

        // --- Session management (extended) ---
        "sessions.create" => methods::sessions::sessions_create(ctx).await,
        "sessions.patch" => methods::sessions::sessions_patch(ctx).await,
        "sessions.compact" => methods::sessions::sessions_compact(ctx).await,
        "sessions.usage" => methods::sessions::sessions_usage(ctx).await,
        "sessions.resolve" => methods::sessions::sessions_resolve(ctx).await,

        // --- Config management ---
        "config.set" => methods::config::config_set(ctx).await,
        "config.patch" => methods::config::config_patch(ctx).await,
        "config.apply" => methods::config::config_apply(ctx).await,
        "config.schema" => methods::config::config_schema(ctx).await,

        // --- Log tailing ---
        "logs.tail" => methods::system::logs_tail(ctx).await,

        // --- Channels ---
        "channels.status" => methods::system::channels_status(ctx).await,

        // --- System (extended) ---
        "system.presence" | "system-presence" => methods::system::system_presence(ctx).await,
        "system.snapshot" => methods::system::system_snapshot(ctx).await,
        "system.update.check" => methods::system::system_update_check(ctx).await,
        "system.update.run" => methods::system::system_update_run(ctx).await,
        "system.shutdown" | "system.stop" => methods::system::system_shutdown(ctx).await,
        "system.restart" => methods::system::system_restart(ctx).await,

        // --- Cron runs ---
        "cron.runs" => methods::system::cron_runs(ctx).await,

        // --- Agent files ---
        "agents.files.list" => methods::agents::agents_files_list(ctx).await,

        // --- Doctor / diagnostics ---
        "doctor.run" => methods::doctor::doctor_run(ctx).await,
        "doctor.memory.status" => methods::doctor::doctor_memory_status(ctx).await,

        // --- Node pairing ---
        "node.pair.request" => methods::node::node_pair_request(ctx).await,
        "node.pair.approve" => methods::node::node_pair_approve(ctx).await,
        "node.pair.reject" => methods::node::node_pair_reject(ctx).await,

        // --- Exec approvals ---
        // Both spellings used by different openclaw UI versions.
        "exec.approvals.get" | "exec.approval.get" => {
            methods::approvals::exec_approval_get(ctx).await
        }
        "exec.approval.set" => methods::approvals::exec_approval_set(ctx).await,
        "exec.approval.resolve" => methods::approvals::exec_approval_resolve(ctx).await,

        // --- Logs ---
        "logs.subscribe" => methods::system::logs_subscribe(ctx).await,

        // --- Web login stubs ---
        "web.login.start" | "web.login.stop" => Ok(serde_json::json!({"ok": true})),

        // --- Skills management stubs ---
        "skills.toggle" | "skills.install" | "skills.uninstall" | "skills.setApiKey" => {
            Ok(serde_json::json!({"ok": true}))
        }

        // --- Update ---
        "update.run" => methods::system::update_run(ctx).await,

        _ => Err(ErrorShape::not_found(format!(
            "unknown method: {}",
            ctx.req.method
        ))),
    }
}

pub fn all_methods() -> Vec<String> {
    vec![
        "sessions.list",
        "sessions.send",
        "sessions.create",
        "sessions.patch",
        "sessions.compact",
        "sessions.usage",
        "sessions.resolve",
        "sessions.messages.subscribe",
        "sessions.messages.unsubscribe",
        "sessions.reset",
        "sessions.delete",
        "chat.send",
        "chat.history",
        "chat.abort",
        "chat.inject",
        "agents.list",
        "agents.create",
        "agents.update",
        "agents.delete",
        "agent.identity.get",
        "agent.send",
        "health",
        "status",
        "models.list",
        "config.get",
        "config.set",
        "config.patch",
        "config.apply",
        "config.schema",
        "cron.list",
        "cron.add",
        "cron.remove",
        "cron.run",
        "cron.update",
        "cron.delete",
        "memory.search",
        "memory.store",
        "memory.status",
        "plugins.list",
        "hooks.list",
        "skills.status",
        "skills.list",
        "skills.bins",
        "skills.toggle",
        "skills.install",
        "skills.uninstall",
        "skills.setApiKey",
        "logs.tail",
        "logs.subscribe",
        "system.update.check",
        "system.update.run",
        "system.shutdown",
        "system.stop",
        "system.restart",
        "update.run",
        "doctor.run",
        "doctor.memory.status",
        "node.list",
        "node.pair.list",
        "node.pair.request",
        "node.pair.approve",
        "node.pair.reject",
        "device.pair.list",
        "device.list",
        "tools.catalog",
        "tools.effective",
        "exec.approval.list",
        "exec.approvals.list",
        "exec.approval.get",
        "exec.approval.set",
        "exec.approval.resolve",
        "exec.approvals.allowlist.get",
        "exec.approvals.allowlist.set",
        "channels.status",
        "system.presence",
        "system.snapshot",
        "cron.runs",
        "agents.files.list",
        "web.login.start",
        "web.login.stop",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}
