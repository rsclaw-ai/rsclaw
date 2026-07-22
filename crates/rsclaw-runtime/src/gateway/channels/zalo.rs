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

pub(crate) fn start_zalo_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &rsclaw_channel::ChannelManager,
    zalo_slot: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::zalo::ZaloChannel>>>,
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
    use rsclaw_channel::zalo::ZaloChannel;

    let Some(zalo_cfg) = &config.channel.channels.zalo else {
        return;
    };
    if !zalo_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy from config (Zalo is DM-only, no group policy needed).
    let dm_policy = zalo_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(rsclaw_config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = zalo_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        rsclaw_channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("zalo", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("zalo".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, access_token) pairs from
    // accounts.<name>.accessToken.
    let mut zalo_accounts: Vec<(String, String)> = Vec::new();
    if let Some(accts) = &zalo_cfg.accounts {
        for (name, acct) in accts {
            let t = acct
                .get("accessToken")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !t.is_empty() {
                zalo_accounts.push((name.clone(), t.to_owned()));
            }
        }
    }

    if zalo_accounts.is_empty() {
        warn!("zalo.accessToken not set in accounts, channel disabled");
        return;
    }

    for (acct_name, access_token) in zalo_accounts {
        let acct_for_log = acct_name.clone();
        let w_acct_outer = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register Zalo channel sender for notification routing.
        {
            let mut senders = channel_senders
                .write()
                .expect("channel_senders lock poisoned");
            senders.insert(format!("zalo/{}", acct_name), out_tx.clone());
            senders
                .entry("zalo".to_string())
                .or_insert_with(|| out_tx.clone());
        }

        let tq = Arc::clone(&task_queue);

        // Per-user inbound queue for Zalo.
        type ZaloItem = (String, String, Vec<rsclaw_agent::registry::ImageAttachment>);
        let zalo_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<ZaloItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender_id: String,
                  text: String,
                  images: Vec<rsclaw_agent::registry::ImageAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let enforcer = Arc::clone(&enforcer);
                let queues = Arc::clone(&zalo_user_queues);
                let tq = Arc::clone(&tq);
                let w_acct_outer = w_acct_outer.clone();
                tokio::spawn(async move {
                    // DM policy check (Zalo is DM-only).
                    {
                        use rsclaw_channel::PolicyResult;
                        match enforcer.check(&sender_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                warn!(peer_id = %sender_id, "zalo DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: sender_id.clone(),
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
                                        target_id: sender_id.clone(),
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
                            let (utx, mut urx) = mpsc::channel::<ZaloItem>(32);
                            map.insert(sender_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_uid = sender_id.clone();
                            let w_tq = Arc::clone(&tq);
                            let w_acct = w_acct_outer.clone();
                            tokio::spawn(async move {
                                while let Some((text, sender_id, images)) = urx.recv().await {
                                    // No debounce — task queue merge_into_pending
                                    // handles rapid consecutive messages automatically.
                                    let handle = match w_reg
                                        .route_account("zalo", Some(&w_acct))
                                        .or_else(|_| w_reg.route_account("zalo", None))
                                    {
                                        Ok(h) => h,
                                        Err(e) => {
                                            error!("zalo route: {e:#}");
                                            continue;
                                        }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: MessageKind::DirectMessage {
                                            account_id: Some(w_acct.clone()),
                                        },
                                        channel: "zalo".to_string(),
                                        peer_id: sender_id.clone(),
                                        dm_scope,
                                    });
                                    let qmsg = crate::gateway::task_queue::QueuedMessage {
                                        text,
                                        sender: sender_id.clone(),
                                        channel: "zalo".to_string(),
                                        chat_id: String::new(),
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
                                        error!(user = %w_uid, "zalo: queue submit failed: {e:#}");
                                    }
                                }
                                debug!(user = %w_uid, "zalo: per-user worker stopped");
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
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let sender_id = sender_id.clone();
                        let w_acct_btw = w_acct_outer.clone();
                        tokio::spawn(async move {
                            let handle = match reg
                                .route_account("zalo", Some(&w_acct_btw))
                                .or_else(|_| reg.route_account("zalo", None))
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
                                        target_id: sender_id,
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
                        let sender_id = sender_id.clone();
                        let w_acct_pp = w_acct_outer.clone();
                        tokio::spawn(async move {
                            let handle = match reg
                                .route_account("zalo", Some(&w_acct_pp))
                                .or_else(|_| reg.route_account("zalo", None))
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
                                channel: "zalo".to_string(),
                                peer_id: sender_id.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(
                                &text,
                                &handle,
                                "zalo",
                                &sender_id,
                                crate::gateway::preparse::PreparseOrigin::User,
                            )
                            .await
                            {
                                reply.target_id = sender_id.clone();
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
                                channel: "zalo".to_string(),
                                peer_id: sender_id.clone(),
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
                            if let Ok(Ok(r)) =
                                tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx)
                                    .await
                            {
                                if !r.is_empty {
                                    if let Err(e) = tx
                                        .send(OutboundMessage {
                                            target_id: sender_id,
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
                        });
                        return;
                    }
                    if let Err(e) = user_tx.try_send((text, sender_id.clone(), images)) {
                        warn!(user = %sender_id, error = %e, "zalo: user queue full, dropping message");
                    }
                });
            },
        );

        let zalo = Arc::new(ZaloChannel::with_api_base(
            access_token,
            zalo_cfg.api_base.clone(),
            on_message,
        ));
        if zalo_slot.set(Arc::clone(&zalo)).is_err() {
            tracing::debug!("slot already set, skipping");
        }
        let zalo_send = Arc::clone(&zalo);
        let shutdown_for_out = shutdown.clone();
        let chan_name = zalo.name().to_owned();
        let cancel_token = manager.register_cancel_token(&chan_name);
        let cancel_for_out = cancel_token.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown_for_out.notified() => {
                        info!("zalo: drain signaled, stopping outbound sender");
                        break;
                    }
                    () = cancel_for_out.cancelled() => {
                        info!("zalo: channel cancelled, stopping outbound sender");
                        break;
                    }
                    msg = out_rx.recv() => {
                        let Some(msg) = msg else { break };
                        if let Err(e) = zalo_send.send(msg).await {
                            error!("zalo send: {e:#}");
                        }
                    }
                }
            }
        });
        if let Err(e) = manager.register(Arc::clone(&zalo) as Arc<dyn Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        let shutdown_for_run = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                res = zalo.run() => {
                    if let Err(e) = res {
                        error!("zalo channel: {e:#}");
                    }
                }
                () = shutdown_for_run.notified() => {
                    info!("zalo: drain signaled, stopping run loop");
                }
                () = cancel_token.cancelled() => {
                    info!("zalo: channel cancelled, stopping run loop");
                }
            }
        });
        info!(account = %acct_for_log, "zalo channel started (webhook mode)");
    } // end for zalo_accounts
}
