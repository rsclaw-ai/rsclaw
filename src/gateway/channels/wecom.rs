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

pub(crate) fn start_wecom_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    wecom_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::wecom::WeComChannel>>>,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
    task_queue: Arc<crate::gateway::task_queue::TaskQueueManager>,
) {
    use crate::channel::wecom::WeComChannel;

    let Some(wc_cfg) = &config.channel.channels.wecom else {
        return;
    };
    if !wc_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Collect (account_name, bot_id, secret, ws_url) tuples.
    let mut wc_accounts: Vec<(String, String, String, Option<String>)> = Vec::new();

    // Legacy: single bot_id/secret at top level.
    if let (Some(bot_id), Some(secret)) = (
        wc_cfg.bot_id.as_deref().filter(|s| !s.is_empty()),
        wc_cfg
            .secret
            .as_ref()
            .and_then(|s| s.resolve_early())
            .filter(|s| !s.is_empty()),
    ) {
        wc_accounts.push((
            "default".to_owned(),
            bot_id.to_owned(),
            secret,
            wc_cfg.ws_url.clone(),
        ));
    }

    // Multi-account: channels.wecom.accounts.<name>.{botId, secret, wsUrl?}
    if let Some(accts) = &wc_cfg.accounts {
        for (name, acct) in accts {
            let bid = acct.get("botId").and_then(|v| v.as_str()).unwrap_or("");
            let sec = acct.get("secret").and_then(|v| v.as_str()).unwrap_or("");
            if !bid.is_empty() && !sec.is_empty() {
                if !wc_accounts.iter().any(|(_, eid, _, _)| eid == bid) {
                    let ws = acct
                        .get("wsUrl")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                        .or_else(|| wc_cfg.ws_url.clone());
                    wc_accounts.push((name.clone(), bid.to_owned(), sec.to_owned(), ws));
                }
            }
        }
    }

    if wc_accounts.is_empty() {
        warn!("wecom bot_id not set, channel disabled");
        return;
    }

    // DM policy enforcement for WeCom.
    let dm_policy = wc_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = wc_cfg.base.allow_from.clone().unwrap_or_default();
    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("wecom", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("wecom".to_owned(), Arc::clone(&enforcer));
    }

    for (acct_name, bot_id, secret, ws_url) in wc_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let tq = Arc::clone(&task_queue);
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Per-user inbound queue for WeCom.
        type WcItem = (
            String,
            String,
            String,
            bool,
            Vec<crate::agent::registry::ImageAttachment>,
            Vec<crate::agent::registry::FileAttachment>,
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
                  images: Vec<crate::agent::registry::ImageAttachment>,
                  files: Vec<crate::agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let tq = Arc::clone(&tq);
                let queues = Arc::clone(&wc_user_queues);
                let enforcer = Arc::clone(&wc_enforcer);
                tokio::spawn(async move {
                    // DM policy check (pairing).
                    if !is_group {
                        match enforcer.check(&from).await {
                            crate::channel::PolicyResult::Allow => {}
                            crate::channel::PolicyResult::SendPairingCode(code) => {
                                let lang = cfg
                                    .raw
                                    .gateway
                                    .as_ref()
                                    .and_then(|g| g.language.as_deref())
                                    .map(crate::i18n::resolve_lang)
                                    .unwrap_or("en");
                                let msg = crate::i18n::t_fmt(
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

                    files: vec![],                                    })
                                    .await
                                {
                                    tracing::warn!("failed to send message: {e}");
                                }
                                return;
                            }
                            crate::channel::PolicyResult::Deny
                            | crate::channel::PolicyResult::PairingQueueFull => {
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
                            tokio::spawn(async move {
                                while let Some((text, from, chat_id, is_group, images, files)) =
                                    urx.recv().await
                                {
                                    // No debounce — task queue merge_into_pending
                                    // handles rapid consecutive messages automatically.
                                    let handle = match w_reg.route("wecom").or_else(|_| w_reg.default_agent()) {
                                        Ok(h) => h,
                                        Err(e) => { error!("wecom route: {e:#}"); continue; }
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
                                        files: files.iter().filter_map(|f| {
                                            crate::gateway::task_queue::stage_file(&f.filename, &f.data, &f.mime_type).ok()
                                        }).collect(),
                                    };
                                    if let Err(e) = w_tq.submit(&session_key, qmsg, crate::gateway::task_queue::Priority::User) {
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
                        tokio::spawn(async move {
                            let handle = match reg.route("wecom").or_else(|_| reg.default_agent()) {
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
                        let cfg = Arc::clone(&cfg);
                        let from = from.clone();
                        let chat_id = chat_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("wecom").or_else(|_| reg.default_agent()) {
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
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "wecom".to_string(),
                                peer_id: from.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(&text, &handle).await {
                                reply.target_id = if is_group { chat_id.clone() } else { from.clone() };
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
                                extra_tools: vec![],
                                images,
                                files,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await {
                                if !r.is_empty {
                                    let target = if is_group { chat_id } else { from };
                                    if let Err(e) = tx.send(OutboundMessage {
                                        target_id: target,
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
                    if let Err(e) =
                        user_tx.try_send((text, from.clone(), chat_id, is_group, images, files))
                    {
                        warn!(user = %from, error = %e, "wecom: user queue full, dropping message");
                    }
                });
            },
        );

        let wecom = Arc::new(WeComChannel::new(bot_id, secret, ws_url, on_message));

        if let Err(e) = manager.register(Arc::clone(&wecom) as Arc<dyn crate::channel::Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        let wecom_send = Arc::clone(&wecom);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = wecom_send.send(msg).await {
                    error!("wecom send: {e:#}");
                }
            }
        });

        // First account fills the webhook slot for backward compatibility.
        if wecom_slot.set(Arc::clone(&wecom)).is_err() {
            tracing::debug!("slot already set, skipping");
        }

        tokio::spawn(async move {
            if let Err(e) = wecom.run().await {
                error!("wecom channel: {e:#}");
            }
        });

        info!(account = %acct_for_log, "wecom AI Bot WS channel started");
    } // end for wc_accounts
}
