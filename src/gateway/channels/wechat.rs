use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::{
    agent::{AgentMessage, AgentRegistry},
    channel::{Channel, OutboundMessage},
    config::runtime::RuntimeConfig,
    gateway::session::{MessageKind, SessionKeyParams, derive_session_key},
};

use super::super::preparse::{
    btw_direct_call, is_fast_preparse, try_preparse_locally,
};
use super::default_dm_scope;


// ---------------------------------------------------------------------------
// WeChat Personal (via ilink)
// ---------------------------------------------------------------------------

/// Per-user sequential message processor for WeChat.
/// Receives inbound messages and submits them to the task queue.
fn spawn_wechat_user_worker(
    user_id: String,
    mut rx: mpsc::Receiver<(
        String,
        Vec<crate::agent::registry::ImageAttachment>,
        Vec<crate::agent::registry::FileAttachment>,
    )>,
    reg: Arc<AgentRegistry>,
    cfg: RuntimeConfig,
    tq: Arc<crate::gateway::task_queue::TaskQueueManager>,
) {
    tokio::spawn(async move {
        debug!(user = %user_id, "wechat: per-user worker started");
        while let Some((text, images, file_attachments)) = rx.recv().await {
            // No debounce — task queue merge_into_pending
            // handles rapid consecutive messages automatically.
            let handle = match reg.route_account("wechat", Some("default")).or_else(|_| reg.default_agent()) {
                Ok(h) => h,
                Err(e) => {
                    error!("wechat route error: {e:#}");
                    continue;
                }
            };
            let dm_scope = default_dm_scope(&cfg);
            let session_key = derive_session_key(&SessionKeyParams {
                agent_id: handle.id.clone(),
                kind: MessageKind::DirectMessage { account_id: None },
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
                files: file_attachments.iter().filter_map(|f| {
                    crate::gateway::task_queue::stage_file(&f.filename, &f.data, &f.mime_type).ok()
                }).collect(),
            };
            if let Err(e) = tq.submit(&session_key, qmsg, crate::gateway::task_queue::Priority::User) {
                error!(user = %user_id, "wechat: queue submit failed: {e:#}");
            }
        }
        debug!(user = %user_id, "wechat: per-user worker stopped");
    });
}

pub(crate) fn start_wechat_personal_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
    task_queue: Arc<crate::gateway::task_queue::TaskQueueManager>,
) {
    // Check if wechat channel is enabled in config
    let enabled = config
        .channel
        .channels
        .wechat
        .as_ref()
        .map(|c| c.base.enabled.unwrap_or(true))
        .unwrap_or(false);

    // Also check for saved token even without explicit config
    let token_data = crate::channel::auth::load_token("wechat");
    let bot_token = if enabled {
        // Try config first, then saved token
        config
            .channel
            .channels
            .wechat
            .as_ref()
            .and_then(|c| c.bot_token.as_ref())
            .and_then(|t| t.as_plain().map(str::to_owned))
            .or_else(|| {
                token_data
                    .as_ref()
                    .and_then(|d| d.get("bot_token"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
    } else if token_data.is_some() {
        // No config but has saved token — auto-enable
        token_data
            .as_ref()
            .and_then(|d| d.get("bot_token"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    } else {
        None
    };

    // Collect (account_name, token) pairs.
    let mut wc_accounts: Vec<(String, String)> = Vec::new();

    if let Some(token) = bot_token {
        wc_accounts.push(("default".to_owned(), token));
    }

    // Multi-account: channels.wechat.accounts.<name>.botToken
    if let Some(accts) = config
        .channel
        .channels
        .wechat
        .as_ref()
        .and_then(|c| c.accounts.as_ref())
    {
        for (name, acct) in accts {
            if let Some(t) = acct.get("botToken").and_then(|v| v.as_str()) {
                if !wc_accounts.iter().any(|(_, et)| et == t) {
                    wc_accounts.push((name.clone(), t.to_owned()));
                }
            }
        }
    }

    if wc_accounts.is_empty() {
        return;
    }

    // Load dmPolicy from config (WeChat is DM-only).
    let dm_policy = config
        .channel
        .channels
        .wechat
        .as_ref()
        .and_then(|c| c.base.dm_policy.clone())
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = config
        .channel
        .channels
        .wechat
        .as_ref()
        .and_then(|c| c.base.allow_from.clone())
        .unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
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

    for (_acct_name, token) in wc_accounts {
        let enforcer = Arc::clone(&enforcer);
        let wechat_base_url = wechat_base_url.clone();
        let reg = Arc::clone(&registry);
        let cfg = config.clone();
        let tq = Arc::clone(&task_queue);

        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register WeChat channel sender for notification routing.
        {
            let mut senders = channel_senders.write().expect("channel_senders lock poisoned");
            senders.insert("wechat".to_string(), out_tx.clone());
        }

        // Per-user inbound queue: serializes messages so each user's messages
        // are processed one at a time, preventing reply channel drops when
        // multiple files/messages arrive in quick succession.
        type InboundItem = (
            String,
            Vec<crate::agent::registry::ImageAttachment>,
            Vec<crate::agent::registry::FileAttachment>,
        );
        let user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<InboundItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |from_user: String,
                  text: String,
                  images: Vec<crate::agent::registry::ImageAttachment>,
                  file_attachments: Vec<crate::agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let tx = out_tx.clone();
                let cfg = cfg.clone();
                let tq = Arc::clone(&tq);
                let queues = Arc::clone(&user_queues);
                let enforcer = Arc::clone(&enforcer);
                tokio::spawn(async move {
                    // DM policy check (WeChat is DM-only).
                    {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&from_user).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %from_user, "wechat DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: from_user.clone(),
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
                                        target_id: from_user.clone(),
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
                        tokio::spawn(async move {
                            let handle = match reg.get("main").or_else(|_| reg.default_agent()) {
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
                                        target_id: from_user,
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
                        let from_user = from_user.clone();
                        tokio::spawn(async move {
                            let handle = match reg.get("main").or_else(|_| reg.default_agent()) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: MessageKind::DirectMessage { account_id: None },
                                channel: "wechat".to_string(),
                                peer_id: from_user.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(&text, &handle, "wechat", &from_user).await {
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
                                        target_id: from_user,
                                        is_group: false,
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
                    // Enqueue — never blocks the poll loop.
                    if let Err(e) = user_tx.try_send((text, images, file_attachments)) {
                        warn!(user = %from_user, error = %e, "wechat: user queue full, dropping message");
                    }
                });
            },
        );

        let wc = Arc::new({
            let ch = crate::channel::wechat::WeChatPersonalChannel::new(token, on_message);
            if let Some(url) = wechat_base_url {
                ch.with_base_url(url)
            } else {
                ch
            }
        });
        if let Err(e) = manager.register(Arc::clone(&wc) as Arc<dyn crate::channel::Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        let wc_send = Arc::clone(&wc);

        tokio::spawn(async move {
            debug!("wechat: outbound sender task started");
            while let Some(msg) = out_rx.recv().await {
                debug!(target = %msg.target_id, text_len = msg.text.len(), "wechat: sending reply");
                if let Err(e) = wc_send.send(msg).await {
                    error!("wechat send error: {e:#}");
                } else {
                    debug!("wechat: reply sent successfully");
                }
            }
        });

        tokio::spawn(async move {
            if let Err(e) = wc.run().await {
                error!("wechat channel error: {e:#}");
            }
        });

        info!("wechat personal channel started");
    } // end for wc_accounts
}
