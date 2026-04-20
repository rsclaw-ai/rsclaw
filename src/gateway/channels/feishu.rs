use std::{sync::Arc, time::Duration};

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::{
    agent::{AgentMessage, AgentRegistry},
    channel::{Channel, OutboundMessage},
    config::runtime::RuntimeConfig,
    gateway::session::{MessageKind, SessionKeyParams, derive_session_key},
};

use super::super::preparse::{
    btw_direct_call, is_fast_preparse, processing_timeout, send_processing,
    try_preparse_locally,
};
use super::super::startup::handle_pending_analysis;
use super::default_dm_scope;

// ---------------------------------------------------------------------------
// Feishu (飞书)
// ---------------------------------------------------------------------------

pub(crate) fn start_feishu_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    feishu_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::feishu::FeishuChannel>>>,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    let fs_cfg = config.channel.channels.feishu.as_ref();
    if let Some(cfg) = fs_cfg {
        if !cfg.base.enabled.unwrap_or(true) {
            return;
        }
    }

    // Collect (account_name, app_id, app_secret, brand) tuples.
    let mut fs_accounts: Vec<(String, String, String, String)> = Vec::new();

    // Legacy: single appId/appSecret at top level.
    if let Some(cfg) = fs_cfg {
        let id = cfg
            .app_id
            .as_deref()
            .filter(|s| !s.starts_with("YOUR_"))
            .map(str::to_owned);
        let secret = cfg
            .app_secret
            .as_ref()
            .and_then(|s| s.as_plain())
            .filter(|s| !s.starts_with("YOUR_"))
            .map(str::to_owned);
        let brand = cfg.brand.as_deref().unwrap_or("feishu").to_owned();
        if let (Some(id), Some(secret)) = (id, secret) {
            fs_accounts.push(("default".to_owned(), id, secret, brand));
        }
    }

    // Saved auth token from onboard flow (fallback for legacy single-account).
    if fs_accounts.is_empty() {
        if let Some(saved) = crate::channel::auth::load_token("feishu") {
            let id = saved["app_id"].as_str().unwrap_or("").to_owned();
            let secret = saved["app_secret"].as_str().unwrap_or("").to_owned();
            let brand = saved["brand"].as_str().unwrap_or("feishu").to_owned();
            if !id.is_empty() && !secret.is_empty() {
                fs_accounts.push(("default".to_owned(), id, secret, brand));
            }
        }
    }

    // Multi-account: channels.feishu.accounts.<name>.{appId, appSecret, brand?}
    if let Some(accts) = fs_cfg.and_then(|c| c.accounts.as_ref()) {
        for (name, acct) in accts {
            let id = acct.get("appId").and_then(|v| v.as_str()).unwrap_or("");
            let secret = acct.get("appSecret").and_then(|v| v.as_str()).unwrap_or("");
            if !id.is_empty() && !secret.is_empty() {
                // Avoid duplicate if top-level credentials == this account's.
                if !fs_accounts.iter().any(|(_, eid, _, _)| eid == id) {
                    let brand = acct
                        .get("brand")
                        .and_then(|v| v.as_str())
                        .unwrap_or("feishu")
                        .to_owned();
                    fs_accounts.push((name.clone(), id.to_owned(), secret.to_owned(), brand));
                }
            }
        }
    }

    if fs_accounts.is_empty() {
        // No config section and no saved token — silently skip.
        if fs_cfg.is_some() {
            warn!("feishu credentials not set, channel disabled");
        }
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = fs_cfg
        .and_then(|c| c.base.dm_policy.clone())
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = fs_cfg
        .and_then(|c| c.base.group_policy.clone())
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = fs_cfg
        .and_then(|c| c.base.group_allow_from.clone())
        .unwrap_or_default();
    let allow_from: Vec<String> = fs_cfg
        .and_then(|c| c.base.allow_from.clone())
        .unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("feishu", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("feishu".to_owned(), Arc::clone(&enforcer));
    }

    let feishu_api_base = fs_cfg.and_then(|c| c.api_base.clone());
    let feishu_ws_url = fs_cfg.and_then(|c| c.ws_url.clone());
    let max_file_size = config
        .ext
        .tools
        .as_ref()
        .and_then(|t| t.upload.as_ref())
        .and_then(|u| u.max_file_size)
        .unwrap_or(128_000_000);

    for (acct_name, app_id, app_secret, brand) in fs_accounts {
        let reg = Arc::clone(&registry);
        let cfg = config.clone();
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register channel sender for notification routing (ACP tools like OpenCode, ClaudeCode)
        {
            let mut senders = _channel_senders.write().expect("channel_senders lock poisoned");
            // Register both "feishu" (for legacy/simple routing) and "feishu/{account}" (for multi-account)
            senders.insert("feishu".to_string(), out_tx.clone());
            senders.insert(format!("feishu/{}", acct_name), out_tx.clone());
        }

        // Find binding for this account to determine which agent handles it.
        let bound_agent = config
            .agents
            .bindings
            .iter()
            .find(|b| {
                b.match_.channel.as_deref() == Some("feishu")
                    && b.match_.account_id.as_deref() == Some(&acct_name)
            })
            .map(|b| b.agent_id.clone());
        let bound = bound_agent.clone();
        let _acct_for_route = acct_name.clone();

        // Per-user inbound queue for Feishu.
        type FsItem = (
            String,
            String,
            String,
            bool,
            Option<String>,
            Vec<crate::agent::registry::ImageAttachment>,
            Vec<crate::agent::registry::FileAttachment>,
        );
        let fs_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<FsItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender_id: String,
                  text: String,
                  chat_id: String,
                  is_group: bool,
                  images: Vec<crate::agent::registry::ImageAttachment>,
                  file_attachments: Vec<crate::agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = cfg.clone();
                let tx = out_tx.clone();
                let bound = bound.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&fs_user_queues);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("feishu group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == chat_id) {
                                    debug!("feishu group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            crate::config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_group {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&sender_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %sender_id, "feishu DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: chat_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t_fmt(
                                            "pairing_required",
                                            crate::i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await
                                {
                                    tracing::warn!("failed to send message: {e}");
                                }
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: chat_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t(
                                            "pairing_queue_full",
                                            crate::i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await
                                {
                                    tracing::warn!("failed to send message: {e}");
                                }
                                return;
                            }
                        }
                    }
                    // Fast preparse bypass: /status, /abort etc. skip per-user queue
                    if is_fast_preparse(&text) {
                        let handle = if let Some(ref agent_id) = bound {
                            match reg.get(agent_id) {
                                Ok(h) => h,
                                Err(_) => match reg.route_account("feishu", None) {
                                    Ok(h) => h,
                                    Err(_) => return,
                                },
                            }
                        } else {
                            match reg.route_account("feishu", None) {
                                Ok(h) => h,
                                Err(_) => return,
                            }
                        };
                        if let Some(mut reply) = try_preparse_locally(&text, &handle).await {
                            reply.target_id = chat_id.clone();
                            reply.is_group = is_group;
                            if !reply.text.is_empty() || !reply.images.is_empty() {
                                if let Err(e) = tx.send(reply).await {

                                    tracing::warn!("failed to send message: {e}");

                                }
                            }
                            return;
                        }
                        // try_preparse_locally returned None (e.g. /clear sets abort
                        // then falls through to agent queue for actual cleanup)
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&sender_id) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&sender_id);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<FsItem>(32);
                            map.insert(sender_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = cfg.clone();
                            let w_tx = tx.clone();
                            let w_uid = sender_id.clone();
                            tokio::spawn(async move {
                                while let Some((
                                    mut text,
                                    sender_id,
                                    chat_id,
                                    is_group,
                                    bound,
                                    mut images,
                                    mut file_attachments,
                                )) = urx.recv().await
                                {
                                    // Debounce: wait briefly then drain queued messages.
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                    while let Ok((extra_text, _, _, _, _, extra_images, extra_files)) = urx.try_recv() {
                                        if !extra_text.is_empty() && !is_fast_preparse(&extra_text) {
                                            text.push('\n');
                                            text.push_str(&extra_text);
                                        }
                                        images.extend(extra_images);
                                        file_attachments.extend(extra_files);
                                    }
                                    info!(user = %w_uid, text_start = %text.chars().take(20).collect::<String>(), "feishu: worker dispatching");
                                    let process_result = tokio::time::timeout(
                                        Duration::from_secs(172800), // 48 hours, matching OpenClaw default
                                        async {
                                    let handle = if let Some(ref agent_id) = bound {
                                        match w_reg.get(agent_id) {
                                            Ok(h) => h,
                                            Err(_) => match w_reg.route_account("feishu", None) {
                                                Ok(h) => h,
                                                Err(e) => { error!("feishu route error: {e:#}"); return; }
                                            },
                                        }
                                    } else {
                                        match w_reg.route_account("feishu", None) {
                                            Ok(h) => h,
                                            Err(e) => { error!("feishu route error: {e:#}"); return; }
                                        }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: if is_group {
                                            MessageKind::GroupMessage {
                                                group_id: chat_id.clone(),
                                                thread_id: None,
                                            }
                                        } else {
                                            MessageKind::DirectMessage { account_id: None }
                                        },
                                        channel: "feishu".to_string(),
                                        peer_id: sender_id.clone(),
                                        dm_scope,
                                    });
                                    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                    let fs_target = if is_group { chat_id.clone() } else { chat_id.clone() };
                                    let msg = AgentMessage {
                                        session_key,
                                        text,
                                        channel: "feishu".to_string(),
                                        peer_id: sender_id.clone(),
                                        chat_id: fs_target.clone(),
                                        reply_tx,
                                        extra_tools: vec![],
                                        images,
                                        files: file_attachments,
                                        is_internal: false,
                                    };
                                    if handle.tx.send(msg).await.is_err() {
                                        error!(user = %sender_id, "feishu: agent channel closed, message dropped");
                                        return;
                                    }
                                    info!(user = %sender_id, "feishu: message sent to agent, waiting for reply");
                                    let reply = tokio::select! {
                                        result = &mut reply_rx => result,
                                        _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                            send_processing(&w_tx, fs_target.clone(), is_group, &w_cfg).await;
                                            reply_rx.await
                                        }
                                    };
                                    match reply {
                                        Ok(r) => {
                                            let pending = r.pending_analysis;
                                            if !r.text.is_empty() || !r.images.is_empty() || !r.files.is_empty() {
                                                if let Err(e) = w_tx
                                                    .send(OutboundMessage {
                                                        target_id: fs_target.clone(),
                                                        is_group,
                                                        text: r.text,
                                                        reply_to: None,
                                                        images: r.images,
                                                        files: r.files,
                                                        channel: None,                                                    })
                                                    .await
                                                {
                                                    tracing::warn!("failed to send message: {e}");
                                                }
                                            }
                                            if let Some(analysis) = pending {
                                                handle_pending_analysis(
                                                    analysis, Arc::clone(&handle), &w_tx,
                                                    fs_target, is_group, &w_cfg,
                                                ).await;
                                            }
                                        }
                                        _ => {}
                                    }
                                        }
                                    ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "feishu: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "feishu: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&sender_id).expect("queue entry must exist").clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = cfg.clone();
                        let question = text[5..].to_owned();
                        let chat_id = chat_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route_account("feishu", None) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &cfg,
                            )
                            .await
                            {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: chat_id,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await
                                {
                                    tracing::warn!("failed to send message: {e}");
                                }
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = cfg.clone();
                        let sender_id = sender_id.clone();
                        let chat_id = chat_id.clone();
                        let bound = bound.clone();
                        tokio::spawn(async move {
                            let handle = if let Some(ref agent_id) = bound {
                                match reg.get(agent_id) {
                                    Ok(h) => h,
                                    Err(_) => match reg.route_account("feishu", None) {
                                        Ok(h) => h,
                                        Err(_) => return,
                                    },
                                }
                            } else {
                                match reg.route_account("feishu", None) {
                                    Ok(h) => h,
                                    Err(_) => return,
                                }
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: chat_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "feishu".to_string(),
                                peer_id: sender_id.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(&text, &handle).await {
                                reply.target_id = chat_id.clone();
                                reply.is_group = is_group;
                                if !reply.text.is_empty() || !reply.images.is_empty() {
                                    if let Err(e) = tx.send(reply).await {

                                        tracing::warn!("failed to send message: {e}");

                                    }
                                }
                                return;
                            }
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let fs_target2 = if is_group { chat_id.clone() } else { chat_id.clone() };
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "feishu".to_string(),
                                peer_id: sender_id,
                                chat_id: fs_target2,
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files: file_attachments,
                            is_internal: false,
                        };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await {
                                if !r.is_empty {
                                    if let Err(e) = tx.send(OutboundMessage {
                                        target_id: chat_id,
                                        is_group,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await
                                    {
                                        tracing::warn!("failed to send message: {e}");
                                    }
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) = user_tx.try_send((
                        text,
                        sender_id.clone(),
                        chat_id,
                        is_group,
                        bound,
                        images,
                        file_attachments,
                    )) {
                        warn!(user = %sender_id, error = %e, "feishu: user queue full, dropping message");
                    }
                });
            },
        );

        let mut fs_channel =
            crate::channel::feishu::FeishuChannel::new(app_id, app_secret, vec![], on_message);
        fs_channel.brand = brand;
        fs_channel.api_base_override = feishu_api_base.clone();
        fs_channel.ws_url_override = feishu_ws_url.clone();
        fs_channel.max_file_size = max_file_size;
        let fs = Arc::new(fs_channel);

        // First account fills the webhook slot for backward compatibility.
        if feishu_slot.set(Arc::clone(&fs)).is_err() {
            tracing::debug!("slot already set, skipping");
        }
        if let Err(e) = manager.register(Arc::clone(&fs) as Arc<dyn crate::channel::Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }

        let fs_send = Arc::clone(&fs);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = fs_send.send(msg).await {
                    error!("feishu send error: {e:#}");
                }
            }
        });

        tokio::spawn(async move {
            if let Err(e) = fs.run().await {
                error!("feishu channel error: {e:#}");
            }
        });

        info!(account = %acct_for_log, "feishu channel started");
    }
}
