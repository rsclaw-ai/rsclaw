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
// QQ Official Bot (QQ机器人)
// ---------------------------------------------------------------------------

pub(crate) fn start_qq_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    let Some(qq_cfg) = &config.channel.channels.qq else {
        return;
    };
    if !qq_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = qq_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = qq_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = qq_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = qq_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("qq", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("qq".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, app_id, app_secret) tuples.
    let mut qq_accounts: Vec<(String, String, String)> = Vec::new();

    // Legacy: single appId/appSecret at top level.
    if let (Some(id), Some(secret)) = (
        qq_cfg.app_id.as_deref().filter(|s| !s.is_empty()),
        qq_cfg
            .app_secret
            .as_ref()
            .and_then(|s| s.as_plain())
            .filter(|s| !s.is_empty()),
    ) {
        qq_accounts.push(("default".to_owned(), id.to_owned(), secret.to_owned()));
    }

    // Multi-account: channels.qq.accounts.<name>.{appId, appSecret}
    if let Some(accts) = &qq_cfg.accounts {
        for (name, acct) in accts {
            let id = acct.get("appId").and_then(|v| v.as_str()).unwrap_or("");
            let secret = acct.get("appSecret").and_then(|v| v.as_str()).unwrap_or("");
            if !id.is_empty() && !secret.is_empty() {
                if !qq_accounts.iter().any(|(_, eid, _)| eid == id) {
                    qq_accounts.push((name.clone(), id.to_owned(), secret.to_owned()));
                }
            }
        }
    }

    if qq_accounts.is_empty() {
        warn!("qq appId not set, channel disabled");
        return;
    }

    let sandbox = qq_cfg.sandbox.unwrap_or(false);
    let intents = qq_cfg.intents;
    let qq_api_base = qq_cfg.api_base.clone();
    let qq_token_url = qq_cfg.token_url.clone();

    for (acct_name, app_id, app_secret) in qq_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let qq_cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());

        // Per-user inbound queue for QQ.
        type QqItem = (
            String,
            String,
            String,
            bool,
            String,
            Vec<crate::agent::registry::ImageAttachment>,
            Vec<crate::agent::registry::FileAttachment>,
        );
        let qq_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<QqItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender_id: String,
                  text: String,
                  target_id: String,
                  is_group: bool,
                  msg_id: String,
                  images: Vec<crate::agent::registry::ImageAttachment>,
                  file_attachments: Vec<crate::agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let tx = out_tx.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&qq_user_queues);
                let qq_cfg = Arc::clone(&qq_cfg_arc);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("qq group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == target_id) {
                                    debug!("qq group message rejected: not in groupAllowFrom");
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
                                debug!(peer_id = %sender_id, "qq DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: target_id.clone(),
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
                                        target_id: target_id.clone(),
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
                            let (utx, mut urx) = mpsc::channel::<QqItem>(32);
                            map.insert(sender_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_tx = tx.clone();
                            let w_uid = sender_id.clone();
                            let w_cfg = Arc::clone(&qq_cfg);
                            tokio::spawn(async move {
                                while let Some((
                                    mut text,
                                    sender_id,
                                    target_id,
                                    is_group,
                                    msg_id,
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
                                    let process_result = tokio::time::timeout(
                                    Duration::from_secs(600),
                                    async {
                                let handle = match w_reg.route("qq").or_else(|_| w_reg.default_agent()) {
                                    Ok(h) => h,
                                    Err(e) => { error!("qq route error: {e:#}"); return; }
                                };
                                let session_key =
                                    format!("qq:{}:{}", if is_group { "group" } else { "dm" }, target_id);
                                let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                let msg = crate::agent::AgentMessage {
                                    session_key,
                                    text,
                                    channel: "qq".to_owned(),
                                    peer_id: sender_id,
                                    chat_id: target_id.clone(),
                                    reply_tx,
                                    extra_tools: vec![],
                                    images,
                                    files: file_attachments,
                                };
                                if handle.tx.send(msg).await.is_err() {
                                    error!("qq: agent inbox closed");
                                    return;
                                }
                                let reply = tokio::select! {
                                    result = &mut reply_rx => result,
                                    _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                        send_processing(&w_tx, target_id.clone(), is_group, &w_cfg).await;
                                        reply_rx.await
                                    }
                                };
                                match reply {
                                    Ok(r) => {
                                        let pending = r.pending_analysis;
                                        if let Err(e) = w_tx
                                            .send(OutboundMessage {
                                                target_id: target_id.clone(),
                                                is_group,
                                                text: r.text,
                                                reply_to: Some(msg_id),
                                                images: r.images,
                                                files: r.files,
                                                channel: None,                                            })
                                            .await
                                        {
                                            tracing::warn!("failed to send message: {e}");
                                        }
                                        if let Some(analysis) = pending {
                                            handle_pending_analysis(
                                                analysis, Arc::clone(&handle), &w_tx,
                                                target_id, is_group, &w_cfg,
                                            ).await;
                                        }
                                    }
                                    Err(_) => error!("qq: agent dropped reply"),
                                }
                                    }
                                ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "qq: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "qq: per-user worker stopped");
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
                        let qq_cfg = Arc::clone(&qq_cfg);
                        let question = text[5..].to_owned();
                        let target_id = target_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("qq").or_else(|_| reg.default_agent()) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &qq_cfg,
                            )
                            .await
                            {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id,
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
                        let qq_cfg = Arc::clone(&qq_cfg);
                        let sender_id = sender_id.clone();
                        let target_id = target_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("qq").or_else(|_| reg.default_agent()) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&qq_cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: target_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "qq".to_string(),
                                peer_id: sender_id.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(&text, &handle).await {
                                reply.target_id = target_id.clone();
                                reply.is_group = is_group;
                                if !reply.text.is_empty() || !reply.images.is_empty() {
                                    if let Err(e) = tx.send(reply).await {

                                        tracing::warn!("failed to send message: {e}");

                                    }
                                }
                                return;
                            }
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "qq".to_string(),
                                peer_id: sender_id,
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files: file_attachments,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await {
                                if !r.is_empty {
                                    if let Err(e) = tx.send(OutboundMessage {
                                        target_id,
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
                        target_id,
                        is_group,
                        msg_id,
                        images,
                        file_attachments,
                    )) {
                        warn!(user = %sender_id, error = %e, "qq: user queue full, dropping message");
                    }
                });
            },
        );

        let qq = Arc::new(crate::channel::qq::QQBotChannel::new_with_overrides(
            app_id,
            app_secret,
            sandbox,
            intents,
            on_message,
            qq_api_base.clone(),
            qq_token_url.clone(),
        ));

        if let Err(e) = manager.register(Arc::clone(&qq) as Arc<dyn crate::channel::Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        let qq_send = Arc::clone(&qq);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = qq_send.send(msg).await {
                    error!("qq send error: {e:#}");
                }
            }
        });

        tokio::spawn(async move {
            if let Err(e) = qq.run().await {
                error!("qq channel error: {e:#}");
            }
        });

        info!(account = %acct_for_log, "qq bot channel started");
    } // end for qq_accounts
}
