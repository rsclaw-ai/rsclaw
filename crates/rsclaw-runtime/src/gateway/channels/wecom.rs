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

pub(crate) fn start_wecom_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut rsclaw_channel::ChannelManager,
    wecom_slot: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::wecom::WeComChannel>>>,
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
    use rsclaw_channel::wecom::WeComChannel;

    let Some(wc_cfg) = &config.channel.channels.wecom else {
        return;
    };
    if !wc_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Collect (account_name, bot_id, secret, ws_url) tuples from
    // accounts.<name>.{botId, secret, wsUrl?}
    let mut wc_accounts: Vec<(String, String, String, Option<String>)> = Vec::new();
    if let Some(accts) = &wc_cfg.accounts {
        for (name, acct) in accts {
            let bid = acct.get("botId").and_then(|v| v.as_str()).unwrap_or("");
            let sec = acct.get("secret").and_then(|v| v.as_str()).unwrap_or("");
            if !bid.is_empty() && !sec.is_empty() {
                let ws = acct
                    .get("wsUrl")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                // Back-compat: fall back to the deprecated top-level ws_url
                // when a per-account wsUrl is not set.
                #[allow(deprecated)]
                let ws = ws.or_else(|| wc_cfg.ws_url.clone());
                wc_accounts.push((name.clone(), bid.to_owned(), sec.to_owned(), ws));
            }
        }
    }

    if wc_accounts.is_empty() {
        warn!("wecom.botId not set in accounts, channel disabled");
        return;
    }

    // DM policy enforcement for WeCom.
    let dm_policy = wc_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(rsclaw_config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = wc_cfg.base.allow_from.clone().unwrap_or_default();
    let enforcer = Arc::new(
        rsclaw_channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("wecom", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("wecom".to_owned(), Arc::clone(&enforcer));
    }

    for (acct_name, bot_id, secret, ws_url) in wc_accounts {
        let acct_for_log = acct_name.clone();
        let w_acct_outer = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let tq = Arc::clone(&task_queue);
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register WeCom channel sender for notification routing.
        {
            let mut senders = channel_senders
                .write()
                .expect("channel_senders lock poisoned");
            senders.insert(format!("wecom/{}", acct_name), out_tx.clone());
            senders
                .entry("wecom".to_string())
                .or_insert_with(|| out_tx.clone());
        }

        // Per-user inbound queue for WeCom.
        type WcItem = (
            String,
            String,
            String,
            bool,
            Vec<rsclaw_agent::registry::ImageAttachment>,
            Vec<rsclaw_agent::registry::FileAttachment>,
        );
        let wc_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<WcItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let wc_enforcer = Arc::clone(&enforcer);
        let on_message = Arc::new(
            move |from: String,
                  text: String,
                  chat_id: String,
                  is_group: bool,
                  images: Vec<rsclaw_agent::registry::ImageAttachment>,
                  files: Vec<rsclaw_agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let tq = Arc::clone(&tq);
                let queues = Arc::clone(&wc_user_queues);
                let enforcer = Arc::clone(&wc_enforcer);
                let w_acct_outer = w_acct_outer.clone();
                tokio::spawn(async move {
                    // DM policy check (pairing).
                    if !is_group {
                        match enforcer.check(&from).await {
                            rsclaw_channel::PolicyResult::Allow => {}
                            rsclaw_channel::PolicyResult::SendPairingCode(code) => {
                                let lang = cfg
                                    .raw
                                    .gateway
                                    .as_ref()
                                    .and_then(|g| g.language.as_deref())
                                    .map(rsclaw_i18n::resolve_lang)
                                    .unwrap_or("en");
                                let msg = rsclaw_i18n::t_fmt(
                                    "pairing_required",
                                    lang,
                                    &[("code", &code)],
                                );
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: chat_id.clone(),
                                        is_group: false,
                                        text: msg,
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
                            rsclaw_channel::PolicyResult::Deny
                            | rsclaw_channel::PolicyResult::PairingQueueFull => {
                                debug!(from = %from, "wecom: DM blocked by policy");
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
                            let (utx, mut urx) = mpsc::channel::<WcItem>(32);
                            map.insert(from.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_uid = from.clone();
                            let w_tq = Arc::clone(&tq);
                            let w_acct = w_acct_outer.clone();
                            tokio::spawn(async move {
                                while let Some((text, from, chat_id, is_group, images, files)) =
                                    urx.recv().await
                                {
                                    // No debounce — task queue merge_into_pending
                                    // handles rapid consecutive messages automatically.
                                    let handle = match w_reg
                                        .route("wecom")
                                        .or_else(|_| w_reg.default_agent())
                                    {
                                        Ok(h) => h,
                                        Err(e) => {
                                            error!("wecom route: {e:#}");
                                            continue;
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
                                            MessageKind::DirectMessage { account_id: Some(w_acct.clone()) }
                                        },
                                        channel: "wecom".to_string(),
                                        peer_id: from.clone(),
                                        dm_scope,
                                    });
                                    let qmsg = crate::gateway::task_queue::QueuedMessage {
                                        text,
                                        sender: from.to_string(),
                                        channel: "wecom".to_string(),
                                        chat_id: chat_id.clone(),
                                        is_group,
                                        reply_to: None,
                                        timestamp: chrono::Utc::now().timestamp(),
                                        images: images.iter().map(|i| i.data.clone()).collect(),
                                        files: files
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
                                        error!(user = %w_uid, "wecom: queue submit failed: {e:#}");
                                    }
                                }
                                debug!(user = %w_uid, "wecom: per-user worker stopped");
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
                        let chat_id = chat_id.clone();
                        let w_acct_btw = w_acct_outer.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route_account("wecom", Some(&w_acct_btw)).or_else(|_| reg.route_account("wecom", None)).or_else(|_| reg.default_agent()) {
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
                                let target = if is_group { chat_id } else { from };
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: target,
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
                        let chat_id = chat_id.clone();
                        let w_acct_pp = w_acct_outer.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route_account("wecom", Some(&w_acct_pp)).or_else(|_| reg.route_account("wecom", None)).or_else(|_| reg.default_agent()) {
                                Ok(h) => h,
                                Err(_) => return,
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
                                    MessageKind::DirectMessage { account_id: Some(w_acct_pp.clone()) }
                                },
                                channel: "wecom".to_string(),
                                peer_id: from.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(
                                &text,
                                &handle,
                                "wecom",
                                &from,
                                crate::gateway::preparse::PreparseOrigin::User,
                            )
                            .await
                            {
                                reply.target_id = if is_group {
                                    chat_id.clone()
                                } else {
                                    from.clone()
                                };
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
                                channel: "wecom".to_string(),
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
                                files,
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
                                    let target = if is_group { chat_id } else { from };
                                    if let Err(e) = tx
                                        .send(OutboundMessage {
                                            target_id: target,
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
                    if let Err(e) =
                        user_tx.try_send((text, from.clone(), chat_id, is_group, images, files))
                    {
                        warn!(user = %from, error = %e, "wecom: user queue full, dropping message");
                    }
                });
            },
        );

        let wecom = Arc::new(WeComChannel::new(bot_id, secret, ws_url, on_message));

        if let Err(e) = manager.register_with_name(format!("wecom/{}", acct_for_log), Arc::clone(&wecom) as Arc<dyn rsclaw_channel::Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        let wecom_send = Arc::clone(&wecom);
        let shutdown_for_out = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown_for_out.notified() => {
                        info!("wecom: drain signaled, stopping outbound sender");
                        break;
                    }
                    msg = out_rx.recv() => {
                        let Some(msg) = msg else { break };
                        if let Err(e) = wecom_send.send(msg).await {
                            error!("wecom send: {e:#}");
                        }
                    }
                }
            }
        });

        // First account fills the webhook slot for backward compatibility.
        if wecom_slot.set(Arc::clone(&wecom)).is_err() {
            tracing::debug!("slot already set, skipping");
        }

        let shutdown_for_run = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                res = wecom.run() => {
                    if let Err(e) = res {
                        error!("wecom channel: {e:#}");
                    }
                }
                () = shutdown_for_run.notified() => {
                    info!("wecom: drain signaled, stopping run loop");
                }
            }
        });

        info!(account = %acct_for_log, "wecom AI Bot WS channel started");
    } // end for wc_accounts
}
