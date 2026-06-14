use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::{
    super::preparse::{btw_direct_call, is_fast_preparse, try_preparse_locally},
    default_dm_scope,
};
use crate::gateway::session::{MessageKind, SessionKeyParams, derive_session_key};
use rsclaw_agent::{AgentMessage, AgentRegistry};
use rsclaw_channel::{Channel, OutboundMessage};
use rsclaw_config::runtime::RuntimeConfig;

// ---------------------------------------------------------------------------
// QQ Official Bot (QQ机器人)
// ---------------------------------------------------------------------------

pub(crate) fn start_qq_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut rsclaw_channel::ChannelManager,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<rsclaw_channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<rsclaw_store::redb_store::RedbStore>,
    channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
    task_queue: Arc<crate::gateway::task_queue::TaskQueueManager>,
    shutdown: crate::gateway::ShutdownCoordinator,
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
        .unwrap_or(rsclaw_config::schema::DmPolicy::Pairing);
    let group_policy = qq_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(rsclaw_config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = qq_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = qq_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        rsclaw_channel::DmPolicyEnforcer::new(dm_policy, allow_from)
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
        let w_acct_outer = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let qq_cfg_arc = Arc::new(config.clone());
        let tq = Arc::clone(&task_queue);
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register QQ channel sender for notification routing.
        {
            let mut senders = channel_senders
                .write()
                .expect("channel_senders lock poisoned");
            senders.insert(format!("qq/{}", acct_name), out_tx.clone());
            senders
                .entry("qq".to_string())
                .or_insert_with(|| out_tx.clone());
        }

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());

        // Per-user inbound queue for QQ.
        type QqItem = (
            String,
            String,
            String,
            bool,
            String,
            Vec<rsclaw_agent::registry::ImageAttachment>,
            Vec<rsclaw_agent::registry::FileAttachment>,
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
                  images: Vec<rsclaw_agent::registry::ImageAttachment>,
                  file_attachments: Vec<rsclaw_agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let tx = out_tx.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&qq_user_queues);
                let qq_cfg = Arc::clone(&qq_cfg_arc);
                let tq = Arc::clone(&tq);
                let w_acct_outer = w_acct_outer.clone();
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            rsclaw_config::schema::GroupPolicy::Disabled => {
                                warn!("qq group message rejected: groupPolicy=disabled");
                                return;
                            }
                            rsclaw_config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == target_id) {
                                    warn!("qq group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            rsclaw_config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_group {
                        use rsclaw_channel::PolicyResult;
                        match enforcer.check(&sender_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                warn!(peer_id = %sender_id, "qq DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: target_id.clone(),
                                        is_group: false,
                                        text: rsclaw_i18n::t_fmt(
                                            "pairing_required",
                                            rsclaw_i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                                        account: Some(w_acct_outer.clone()),
                                        files: vec![],
                                    })
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
                                        text: rsclaw_i18n::t(
                                            "pairing_queue_full",
                                            rsclaw_i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                                        account: Some(w_acct_outer.clone()),
                                        files: vec![],
                                    })
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
                            let w_uid = sender_id.clone();
                            let w_tq = Arc::clone(&tq);
                            let w_acct = w_acct_outer.clone();
                            tokio::spawn(async move {
                                while let Some((
                                    text,
                                    sender_id,
                                    target_id,
                                    is_group,
                                    msg_id,
                                    images,
                                    file_attachments,
                                )) = urx.recv().await
                                {
                                    // No debounce — task queue merge_into_pending
                                    // handles rapid consecutive messages automatically.
                                    let session_key = format!(
                                        "qq:{}:{}",
                                        if is_group { "group" } else { "dm" },
                                        target_id
                                    );
                                    let qmsg = crate::gateway::task_queue::QueuedMessage {
                                        text,
                                        sender: sender_id.clone(),
                                        channel: "qq".to_string(),
                                        chat_id: target_id.clone(),
                                        is_group,
                                        reply_to: Some(msg_id),
                                        timestamp: chrono::Utc::now().timestamp(),
                                        images: images.iter().map(|i| i.data.clone()).collect(),
                                        files: file_attachments
                                            .iter()
                                            .filter_map(|f| {
                                                crate::gateway::task_queue::stage_file(
                                                    &f.filename,
                                                    &f.data,
                                                    &f.mime_type,
                                                )
                                                .ok()
                                            })
                                            .collect(),
                                        account: Some(w_acct.clone()),
                                    };
                                    if let Err(e) = w_tq.submit(
                                        &session_key,
                                        qmsg,
                                        crate::gateway::task_queue::Priority::User,
                                    ) {
                                        error!(user = %w_uid, "qq: queue submit failed: {e:#}");
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
                        let w_acct_btw = w_acct_outer.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route_account("qq", Some(&w_acct_btw)).or_else(|_| reg.route_account("qq", None)).or_else(|_| reg.default_agent()) {
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

                                        account: Some(w_acct_btw.clone()),
                                        files: vec![],
                                    })
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
                        let w_acct_pp = w_acct_outer.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route_account("qq", Some(&w_acct_pp)).or_else(|_| reg.route_account("qq", None)).or_else(|_| reg.default_agent()) {
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
                                    MessageKind::DirectMessage { account_id: Some(w_acct_pp.clone()) }
                                },
                                channel: "qq".to_string(),
                                peer_id: sender_id.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(
                                &text,
                                &handle,
                                "qq",
                                &sender_id,
                                crate::gateway::preparse::PreparseOrigin::User,
                            )
                            .await
                            {
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
                                task_id: None,
                                context_id: None,
                                event_tx: None,
                                cancel_token: None,
                                input_request_tx: None,
                                extra_tools: vec![],
                                images,
                                files: file_attachments,
                                account: Some(w_acct_pp.clone()),
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(Ok(r)) =
                                tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx)
                                    .await
                            {
                                if !r.is_empty {
                                    if let Err(e) = tx
                                        .send(OutboundMessage {
                                            target_id,
                                            is_group,
                                            text: r.text,
                                            reply_to: None,
                                            images: r.images,
                                            files: r.files,
                                            channel: None,
                                            account: Some(w_acct_pp.clone()),
                                        })
                                        .await
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

        let qq = Arc::new(rsclaw_channel::qq::QQBotChannel::new_with_overrides(
            app_id,
            app_secret,
            sandbox,
            intents,
            on_message,
            qq_api_base.clone(),
            qq_token_url.clone(),
        ));

        if let Err(e) = manager.register_with_name(format!("qq/{}", acct_for_log), Arc::clone(&qq) as Arc<dyn rsclaw_channel::Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        let qq_send = Arc::clone(&qq);
        let shutdown_for_out = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown_for_out.notified() => {
                        info!("qq: drain signaled, stopping outbound sender");
                        break;
                    }
                    msg = out_rx.recv() => {
                        let Some(msg) = msg else { break };
                        if let Err(e) = qq_send.send(msg).await {
                            error!("qq send error: {e:#}");
                        }
                    }
                }
            }
        });

        let shutdown_for_run = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                res = qq.run() => {
                    if let Err(e) = res {
                        error!("qq channel error: {e:#}");
                    }
                }
                () = shutdown_for_run.notified() => {
                    info!("qq: drain signaled, stopping run loop");
                }
            }
        });

        info!(account = %acct_for_log, "qq bot channel started");
    } // end for qq_accounts
}
