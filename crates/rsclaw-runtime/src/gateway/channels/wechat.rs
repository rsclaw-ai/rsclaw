use std::sync::Arc;

use rsclaw_agent::{AgentMessage, AgentRegistry};
use rsclaw_channel::{Channel, OutboundMessage};
use rsclaw_config::runtime::RuntimeConfig;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::{
    super::preparse::{btw_direct_call, is_fast_preparse, try_preparse_locally},
    default_dm_scope,
};
use crate::gateway::session::{MessageKind, SessionKeyParams, derive_session_key};

// ---------------------------------------------------------------------------
// WeChat Personal (via ilink)
// ---------------------------------------------------------------------------

/// Per-user sequential message processor for WeChat.
/// Receives inbound messages and submits them to the task queue.
fn spawn_wechat_user_worker(
    user_id: String,
    w_acct: String,
    mut rx: mpsc::Receiver<(
        String,
        Vec<rsclaw_agent::registry::ImageAttachment>,
        Vec<rsclaw_agent::registry::FileAttachment>,
    )>,
    reg: Arc<AgentRegistry>,
    cfg: RuntimeConfig,
    tq: Arc<crate::gateway::task_queue::TaskQueueManager>,
) {
    tokio::spawn(async move {
        debug!(user = %user_id, account = %w_acct, "wechat: per-user worker started");
        while let Some((text, images, file_attachments)) = rx.recv().await {
            let handle = match reg
                .route_account("wechat", Some(&w_acct))
                .or_else(|_| reg.route_account("wechat", None))
                .or_else(|_| reg.default_agent())
            {
                Ok(h) => h,
                Err(e) => {
                    error!("wechat route error: {e:#}");
                    continue;
                }
            };
            let dm_scope = default_dm_scope(&cfg);
            let session_key = derive_session_key(&SessionKeyParams {
                agent_id: handle.id.clone(),
                kind: MessageKind::DirectMessage {
                    account_id: Some(w_acct.clone()),
                },
                channel: "wechat".to_string(),
                peer_id: user_id.clone(),
                dm_scope,
            });
            let qmsg = crate::gateway::task_queue::QueuedMessage {
                text,
                sender: user_id.to_string(),
                channel: "wechat".to_string(),
                chat_id: String::new(),
                is_group: false,
                reply_to: None,
                timestamp: chrono::Utc::now().timestamp(),
                images: images.iter().map(|i| i.data.clone()).collect(),
                files: file_attachments
                    .iter()
                    .filter_map(|f| {
                        crate::gateway::task_queue::stage_file(&f.filename, &f.data, &f.mime_type)
                            .ok()
                    })
                    .collect(),
                account: Some(w_acct.clone()),
            };
            if let Err(e) = tq.submit(
                &session_key,
                qmsg,
                crate::gateway::task_queue::Priority::User,
            ) {
                error!(user = %user_id, "wechat: queue submit failed: {e:#}");
            }
        }
        debug!(user = %user_id, "wechat: per-user worker stopped");
    });
}

pub(crate) fn start_wechat_personal_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &rsclaw_channel::ChannelManager,
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
    // Collect (account_name, token) pairs from:
    //   1. channels.wechat.accounts.<name>.botToken  (config file)
    //   2. saved token file (QR login fallback, for backward compat)
    let mut wc_accounts: Vec<(String, String)> = Vec::new();

    // 1. Config file: accounts.<name>.botToken
    if let Some(accts) = config
        .channel
        .channels
        .wechat
        .as_ref()
        .and_then(|c| c.accounts.as_ref())
    {
        for (name, acct) in accts {
            if let Some(t) = acct.get("botToken").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    wc_accounts.push((name.clone(), t.to_owned()));
                }
            }
        }
    }

    // 2. Saved token from QR login (legacy fallback, only if no accounts)
    if wc_accounts.is_empty() {
        if let Some(saved) = rsclaw_channel::auth::load_token("wechat") {
            if let Some(token) = saved.get("bot_token").and_then(|v| v.as_str()) {
                if !token.is_empty() {
                    wc_accounts.push(("default".to_owned(), token.to_owned()));
                    tracing::info!("wechat: using saved token from QR login");
                }
            }
        }
    }

    if wc_accounts.is_empty() {
        warn!("wechat.botToken not set in accounts, channel disabled");
        return;
    }

    // Load dmPolicy from config (WeChat is DM-only).
    let dm_policy = config
        .channel
        .channels
        .wechat
        .as_ref()
        .and_then(|c| c.base.dm_policy.clone())
        .unwrap_or(rsclaw_config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = config
        .channel
        .channels
        .wechat
        .as_ref()
        .and_then(|c| c.base.allow_from.clone())
        .unwrap_or_default();

    let enforcer = Arc::new(
        rsclaw_channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("wechat", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("wechat".to_owned(), Arc::clone(&enforcer));
    }

    let wechat_base_url = config
        .channel
        .channels
        .wechat
        .as_ref()
        .and_then(|c| c.base_url.as_deref())
        .map(str::to_owned);

    for (acct_name, token) in wc_accounts {
        let enforcer = Arc::clone(&enforcer);
        let wechat_base_url = wechat_base_url.clone();
        let reg = Arc::clone(&registry);
        let cfg = config.clone();
        let tq = Arc::clone(&task_queue);
        let shutdown_for_messages = shutdown.clone();
        let w_acct_outer = acct_name.clone();

        // Find binding for this account to determine which agent handles it.
        let bound_agent = config
            .agents
            .bindings
            .iter()
            .find(|b| {
                b.match_.channel.as_deref() == Some("wechat")
                    && b.match_.account_id.as_deref() == Some(&acct_name)
            })
            .map(|b| b.agent_id.clone());

        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register channel sender for notification routing.
        // - "wechat/{account}" is the canonical key — multi-account-aware callers use
        //   it.
        // - bare "wechat" is registered only by the first account so legacy callers
        //   still find a sender. Without this guard each account overwrote the bare
        //   key, leaving the last-registered account routing replies for messages
        //   received via every other account.
        {
            let mut senders = channel_senders
                .write()
                .expect("channel_senders lock poisoned");
            senders.insert(format!("wechat/{}", acct_name), out_tx.clone());
            senders
                .entry("wechat".to_string())
                .or_insert_with(|| out_tx.clone());
        }

        // Per-user inbound queue: serializes messages so each user's messages
        // are processed one at a time, preventing reply channel drops when
        // multiple files/messages arrive in quick succession.
        type InboundItem = (
            String,
            Vec<rsclaw_agent::registry::ImageAttachment>,
            Vec<rsclaw_agent::registry::FileAttachment>,
        );
        let user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<InboundItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |from_user: String,
                  text: String,
                  images: Vec<rsclaw_agent::registry::ImageAttachment>,
                  file_attachments: Vec<rsclaw_agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let tx = out_tx.clone();
                let cfg = cfg.clone();
                let tq = Arc::clone(&tq);
                let queues = Arc::clone(&user_queues);
                let enforcer = Arc::clone(&enforcer);
                let shutdown = shutdown_for_messages.clone();
                let bound = bound_agent.clone();
                let w_acct = w_acct_outer.clone();
                tokio::spawn(async move {
                    if shutdown.is_draining() {
                        debug!(peer_id = %from_user, "wechat: dropping inbound message during drain");
                        return;
                    }
                    // DM policy check (WeChat is DM-only).
                    {
                        use rsclaw_channel::PolicyResult;
                        match enforcer.check(&from_user).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                warn!(peer_id = %from_user, "wechat DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: from_user.clone(),
                                        is_group: false,
                                        text: rsclaw_i18n::t_fmt(
                                            "pairing_required",
                                            rsclaw_i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                                        account: Some(w_acct.clone()),
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
                                        target_id: from_user.clone(),
                                        is_group: false,
                                        text: rsclaw_i18n::t(
                                            "pairing_queue_full",
                                            rsclaw_i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                                        account: Some(w_acct.clone()),
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
                        if let Some(existing) = map.get(&from_user) {
                            if !existing.is_closed() {
                                existing.clone()
                            } else {
                                // Channel closed, create new one.
                                map.remove(&from_user);
                                let (utx, urx) = mpsc::channel::<InboundItem>(32);
                                map.insert(from_user.clone(), utx.clone());
                                // Spawn per-user sequential processor.
                                spawn_wechat_user_worker(
                                    from_user.clone(),
                                    w_acct.clone(),
                                    urx,
                                    Arc::clone(&reg),
                                    cfg.clone(),
                                    Arc::clone(&tq),
                                );
                                utx
                            }
                        } else {
                            let (utx, urx) = mpsc::channel::<InboundItem>(32);
                            map.insert(from_user.clone(), utx.clone());
                            spawn_wechat_user_worker(
                                from_user.clone(),
                                w_acct.clone(),
                                urx,
                                Arc::clone(&reg),
                                cfg.clone(),
                                Arc::clone(&tq),
                            );
                            utx
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = cfg.clone();
                        let question = text[5..].to_owned();
                        let from_user = from_user.clone();
                        let bound = bound.clone();
                        let w_acct = w_acct.clone();
                        tokio::spawn(async move {
                            let handle = if let Some(ref agent_id) = bound {
                                match reg.get(agent_id) {
                                    Ok(h) => h,
                                    Err(_) => match reg.route_account("wechat", Some(&w_acct)) {
                                        Ok(h) => h,
                                        Err(_) => return,
                                    },
                                }
                            } else {
                                match reg.route_account("wechat", Some(&w_acct)) {
                                    Ok(h) => h,
                                    Err(_) => return,
                                }
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
                                        target_id: from_user,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                                        account: Some(w_acct),
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
                        let cfg = cfg.clone();
                        let from_user = from_user.clone();
                        let bound = bound.clone();
                        let w_acct = w_acct.clone();
                        tokio::spawn(async move {
                            let handle = if let Some(ref agent_id) = bound {
                                match reg.get(agent_id) {
                                    Ok(h) => h,
                                    Err(_) => match reg.route_account("wechat", Some(&w_acct)) {
                                        Ok(h) => h,
                                        Err(_) => return,
                                    },
                                }
                            } else {
                                match reg.route_account("wechat", Some(&w_acct)) {
                                    Ok(h) => h,
                                    Err(_) => return,
                                }
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: MessageKind::DirectMessage {
                                    account_id: Some(w_acct.clone()),
                                },
                                channel: "wechat".to_string(),
                                peer_id: from_user.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(
                                &text,
                                &handle,
                                "wechat",
                                &from_user,
                                crate::gateway::preparse::PreparseOrigin::User,
                            )
                            .await
                            {
                                reply.target_id = from_user.clone();
                                reply.is_group = false;
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
                                channel: "wechat".to_string(),
                                peer_id: from_user.clone(),
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
                                account: Some(w_acct.clone()),
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(10),
                                reply_rx,
                            )
                            .await
                            {
                                Ok(Ok(r)) => {
                                    if !r.is_empty {
                                        if let Err(e) = tx
                                            .send(OutboundMessage {
                                                target_id: from_user,
                                                is_group: false,
                                                text: r.text,
                                                reply_to: None,
                                                images: r.images,
                                                files: r.files,
                                                channel: None,
                                                account: Some(w_acct),
                                            })
                                            .await
                                        {
                                            tracing::warn!("failed to send message: {e}");
                                        }
                                    }
                                }
                                Ok(Err(_)) => {
                                    warn!("wechat: chat-mode agent reply error");
                                    let _ = tx
                                        .send(OutboundMessage {
                                            target_id: from_user.clone(),
                                            is_group: false,
                                            text: rsclaw_i18n::t(
                                                "chat_reply_error",
                                                rsclaw_i18n::default_lang(),
                                            ),
                                            reply_to: None,
                                            images: vec![],
                                            files: vec![],
                                            channel: None,
                                            account: Some(w_acct.clone()),
                                        })
                                        .await;
                                }
                                Err(_) => {
                                    warn!("wechat: chat-mode agent reply timed out");
                                    let _ = tx
                                        .send(OutboundMessage {
                                            target_id: from_user.clone(),
                                            is_group: false,
                                            text: rsclaw_i18n::t(
                                                "chat_reply_timeout",
                                                rsclaw_i18n::default_lang(),
                                            ),
                                            reply_to: None,
                                            images: vec![],
                                            files: vec![],
                                            channel: None,
                                            account: Some(w_acct.clone()),
                                        })
                                        .await;
                                }
                            }
                        });
                        return;
                    }
                    // Enqueue — never blocks the poll loop.
                    if let Err(e) = user_tx.try_send((text, images, file_attachments)) {
                        warn!(user = %from_user, error = %e, "wechat: user queue full, dropping message");
                    }
                });
            },
        );

        let wc = Arc::new({
            let ch = rsclaw_channel::wechat::WeChatPersonalChannel::new(token, on_message);
            if let Some(url) = wechat_base_url {
                ch.with_base_url(url)
            } else {
                ch
            }
        });
        let acct_key = format!("wechat/{}", acct_name);
        if let Err(e) = manager.register_with_name(
            acct_key.clone(),
            Arc::clone(&wc) as Arc<dyn rsclaw_channel::Channel>,
        ) {
            tracing::warn!("failed to register channel `{acct_key}`: {e}");
        }
        if manager.get("wechat").is_none()
            && let Err(e) = manager.register_with_name(
                "wechat".to_owned(),
                Arc::clone(&wc) as Arc<dyn rsclaw_channel::Channel>,
            )
        {
            tracing::warn!("failed to register bare `wechat` channel: {e}");
        }
        let cancel_token = manager.register_cancel_token(&acct_key);
        let wc_send = Arc::clone(&wc);
        let shutdown_for_outbound = shutdown.clone();
        let cancel_for_out = cancel_token.clone();

        tokio::spawn(async move {
            debug!("wechat: outbound sender task started");
            // Per-target throttle: wechat ilink returns ret=-2 (anti-spam)
            // when two notifications hit the same recipient within ~1s.
            // Keep one timestamp per target_id; sleep the deficit before
            // sending, listening to drain so restart isn't blocked.
            let mut last_send: std::collections::HashMap<String, std::time::Instant> =
                std::collections::HashMap::new();
            const PER_TARGET_INTERVAL: std::time::Duration = std::time::Duration::from_millis(3000);
            let cancel_for_throttle = cancel_for_out.clone();
            loop {
                tokio::select! {
                    () = shutdown_for_outbound.notified() => {
                        info!("wechat: drain signaled, stopping outbound sender");
                        break;
                    }
                    () = cancel_for_out.cancelled() => {
                        info!("wechat: channel cancelled, stopping outbound sender");
                        break;
                    }
                    msg = out_rx.recv() => {
                        let Some(msg) = msg else { break };
                        if let Some(prev) = last_send.get(&msg.target_id)
                            && let Some(wait) = PER_TARGET_INTERVAL.checked_sub(prev.elapsed())
                        {
                            tokio::select! {
                                _ = tokio::time::sleep(wait) => {}
                                () = shutdown_for_outbound.notified() => {
                                    info!("wechat: drain during throttle wait, dropping message");
                                    break;
                                }
                                () = cancel_for_throttle.cancelled() => {
                                    info!("wechat: channel cancelled during throttle wait, dropping message");
                                    break;
                                }
                            }
                        }
                        let target = msg.target_id.clone();
                        debug!(target = %target, text_len = msg.text.len(), "wechat: sending reply");
                        if let Err(e) = wc_send.send(msg).await {
                            error!("wechat send error: {e:#}");
                        } else {
                            debug!("wechat: reply sent successfully");
                        }
                        last_send.insert(target, std::time::Instant::now());
                    }
                }
            }
        });

        let shutdown_for_poll = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                res = wc.run() => {
                    if let Err(e) = res {
                        error!("wechat channel error: {e:#}");
                    }
                }
                () = shutdown_for_poll.notified() => {
                    info!("wechat: drain signaled, stopping long-poll loop");
                }
                () = cancel_token.cancelled() => {
                    info!("wechat: channel cancelled, stopping long-poll loop");
                }
            }
        });

        info!(account = %acct_name, "wechat personal channel started");
    } // end for wc_accounts
}
