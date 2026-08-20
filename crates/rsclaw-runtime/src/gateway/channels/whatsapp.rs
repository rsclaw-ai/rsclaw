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

pub(crate) fn start_whatsapp_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &rsclaw_channel::ChannelManager,
    whatsapp_slot: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::whatsapp::WhatsAppChannel>>>,
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
    use rsclaw_channel::whatsapp::WhatsAppChannel;

    let Some(wa_cfg) = &config.channel.channels.whatsapp else {
        return;
    };
    if !wa_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy from config (WhatsApp is DM-only, no group policy needed).
    let dm_policy = wa_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(rsclaw_config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = wa_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        rsclaw_channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("whatsapp", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("whatsapp".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, phone_number_id, access_token) tuples from
    // accounts.<name>.{phoneNumberId, accessToken}
    let mut wa_accounts: Vec<(String, String, String)> = Vec::new();
    if let Some(accts) = &wa_cfg.accounts {
        for (name, acct) in accts {
            let pid = acct
                .get("phoneNumberId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let token = acct
                .get("accessToken")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !pid.is_empty() && !token.is_empty() {
                wa_accounts.push((name.clone(), pid.to_owned(), token.to_owned()));
            }
        }
    }

    if wa_accounts.is_empty() {
        warn!("whatsapp.phoneNumberId not set in accounts, channel disabled");
        return;
    }

    for (acct_name, phone_number_id, access_token) in wa_accounts {
        let acct_for_log = acct_name.clone();
        let w_acct_outer = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let tq = Arc::clone(&task_queue);
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register WhatsApp channel sender for notification routing.
        {
            let mut senders = channel_senders
                .write()
                .expect("channel_senders lock poisoned");
            senders.insert(format!("whatsapp/{}", acct_name), out_tx.clone());
            senders
                .entry("whatsapp".to_string())
                .or_insert_with(|| out_tx.clone());
        }

        // Per-user inbound queue for WhatsApp.
        type WaItem = (String, String, Vec<rsclaw_agent::registry::ImageAttachment>);
        let wa_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<WaItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |from: String,
                  text: String,
                  images: Vec<rsclaw_agent::registry::ImageAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let enforcer = Arc::clone(&enforcer);
                let queues = Arc::clone(&wa_user_queues);
                let tq = Arc::clone(&tq);
                let w_acct_outer = w_acct_outer.clone();
                tokio::spawn(async move {
                    // DM policy check (WhatsApp is DM-only).
                    {
                        use rsclaw_channel::PolicyResult;
                        match enforcer.check(&from).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                warn!(peer_id = %from, "whatsapp DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: from.clone(),
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
                                        target_id: from.clone(),
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
                        let needs_create = match map.get(&from) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&from);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<WaItem>(32);
                            map.insert(from.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_tq = Arc::clone(&tq);
                            let w_uid = from.clone();
                            let w_acct = w_acct_outer.clone();
                            tokio::spawn(async move {
                                while let Some((text, from, images)) = urx.recv().await {
                                    // No debounce -- task queue merge_into_pending
                                    // handles rapid consecutive messages automatically.
                                    let handle = match w_reg
                                        .route_account("whatsapp", Some(&w_acct))
                                        .or_else(|_| w_reg.route_account("whatsapp", None))
                                    {
                                        Ok(h) => h,
                                        Err(e) => {
                                            error!("whatsapp route: {e:#}");
                                            continue;
                                        }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: MessageKind::DirectMessage {
                                            account_id: Some(w_acct.clone()),
                                        },
                                        channel: "whatsapp".to_string(),
                                        peer_id: from.clone(),
                                        dm_scope,
                                    });
                                    let qmsg = crate::gateway::task_queue::QueuedMessage {
                                        text,
                                        sender: from.to_string(),
                                        channel: "whatsapp".to_string(),
                                        chat_id: from.to_string(),
                                        is_group: false,
                                        reply_to: None,
                                        timestamp: chrono::Utc::now().timestamp(),
                                        images: images.iter().map(|i| i.data.clone()).collect(),
                                        files: vec![],
                                        account: Some(w_acct.clone()),
                                    };
                                    if let Err(e) = w_tq.submit(
                                        &session_key,
                                        qmsg,
                                        crate::gateway::task_queue::Priority::User,
                                    ) {
                                        error!(user = %w_uid, "whatsapp: queue submit failed: {e:#}");
                                    }
                                }
                                debug!(user = %w_uid, "whatsapp: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&from).expect("queue entry must exist").clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let from = from.clone();
                        let w_acct_btw = w_acct_outer.clone();
                        tokio::spawn(async move {
                            let handle = match reg
                                .route_account("whatsapp", Some(&w_acct_btw))
                                .or_else(|_| reg.route_account("whatsapp", None))
                            {
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
                                        target_id: from,
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
                        let cfg = Arc::clone(&cfg);
                        let from = from.clone();
                        let w_acct_pp = w_acct_outer.clone();
                        tokio::spawn(async move {
                            let handle = match reg
                                .route_account("whatsapp", Some(&w_acct_pp))
                                .or_else(|_| reg.route_account("whatsapp", None))
                            {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: MessageKind::DirectMessage {
                                    account_id: Some(w_acct_pp.clone()),
                                },
                                channel: "whatsapp".to_string(),
                                peer_id: from.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(
                                &text,
                                &handle,
                                "whatsapp",
                                &from,
                                crate::gateway::preparse::PreparseOrigin::User,
                            )
                            .await
                            {
                                reply.target_id = from.clone();
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
                                channel: "whatsapp".to_string(),
                                peer_id: from.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                task_id: None,
                                context_id: None,
                                event_tx: None,
                                cancel_token: None,
                                input_request_tx: None,
                                extra_tools: vec![],
                                images,
                                files: vec![],
                                account: Some(w_acct_pp.clone()),
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            match tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx)
                                .await
                            {
                                Ok(Ok(r)) => {
                                    if !r.is_empty {
                                        if let Err(e) = tx
                                            .send(OutboundMessage {
                                                target_id: from,
                                                is_group: false,
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
                                Ok(Err(_)) => {
                                    warn!("whatsapp: chat-mode agent reply error");
                                    let _ = tx
                                        .send(OutboundMessage {
                                            target_id: from.clone(),
                                            is_group: false,
                                            text: rsclaw_i18n::t(
                                                "chat_reply_error",
                                                rsclaw_i18n::default_lang(),
                                            ),
                                            reply_to: None,
                                            images: vec![],
                                            files: vec![],
                                            channel: None,
                                            account: Some(w_acct_pp.clone()),
                                        })
                                        .await;
                                }
                                Err(_) => {
                                    warn!("whatsapp: chat-mode agent reply timed out");
                                    let _ = tx
                                        .send(OutboundMessage {
                                            target_id: from.clone(),
                                            is_group: false,
                                            text: rsclaw_i18n::t(
                                                "chat_reply_timeout",
                                                rsclaw_i18n::default_lang(),
                                            ),
                                            reply_to: None,
                                            images: vec![],
                                            files: vec![],
                                            channel: None,
                                            account: Some(w_acct_pp.clone()),
                                        })
                                        .await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) = user_tx.try_send((text, from.clone(), images)) {
                        warn!(user = %from, error = %e, "whatsapp: user queue full, dropping message");
                    }
                });
            },
        );

        let wa = Arc::new(WhatsAppChannel::with_api_base(
            phone_number_id,
            access_token,
            wa_cfg.api_base.clone(),
            on_message,
        ));
        if whatsapp_slot.set(Arc::clone(&wa)).is_err() {
            tracing::debug!("slot already set, skipping");
        }
        let wa_send = Arc::clone(&wa);
        let shutdown_for_out = shutdown.clone();
        let acct_key = format!("whatsapp/{}", acct_name);
        let cancel_token = manager.register_cancel_token(&acct_key);
        let cancel_for_out = cancel_token.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown_for_out.notified() => {
                        info!("whatsapp: drain signaled, stopping outbound sender");
                        break;
                    }
                    () = cancel_for_out.cancelled() => {
                        info!("whatsapp: channel cancelled, stopping outbound sender");
                        break;
                    }
                    msg = out_rx.recv() => {
                        let Some(msg) = msg else { break };
                        if let Err(e) = wa_send.send(msg).await {
                            error!("whatsapp send: {e:#}");
                        }
                    }
                }
            }
        });
        if let Err(e) = manager.register_with_name(acct_key, Arc::clone(&wa) as Arc<dyn Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        let shutdown_for_run = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                res = wa.run() => {
                    if let Err(e) = res {
                        error!("whatsapp channel: {e:#}");
                    }
                }
                () = shutdown_for_run.notified() => {
                    info!("whatsapp: drain signaled, stopping run loop");
                }
                () = cancel_token.cancelled() => {
                    info!("whatsapp: channel cancelled, stopping run loop");
                }
            }
        });
        info!(account = %acct_for_log, "whatsapp channel started (webhook mode)");
    } // end for wa_accounts
}
