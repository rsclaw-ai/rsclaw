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
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: chat_id.clone(),
                                        is_group: false,
                                        text: msg,
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
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
                            let w_tx = tx.clone();
                            let w_uid = from.clone();
                            tokio::spawn(async move {
                                while let Some((mut text, from, chat_id, is_group, mut images, mut files)) =
                                    urx.recv().await
                                {
                                    // Debounce: wait briefly then drain queued messages.
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                    while let Ok((extra_text, _, _, _, extra_images, extra_files)) = urx.try_recv() {
                                        if !extra_text.is_empty() && !is_fast_preparse(&extra_text) {
                                            text.push('\n');
                                            text.push_str(&extra_text);
                                        }
                                        images.extend(extra_images);
                                        files.extend(extra_files);
                                    }
                                    let process_result = tokio::time::timeout(
                                Duration::from_secs(600),
                                async {
                            let handle = match w_reg.route("wecom").or_else(|_| w_reg.default_agent()) {
                                Ok(h) => h,
                                Err(e) => { error!("wecom route: {e:#}"); return; }
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
                            let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                            let msg = crate::agent::AgentMessage {
                                session_key,
                                text,
                                channel: "wecom".to_owned(),
                                peer_id: from.clone(),
                                chat_id: chat_id.clone(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            let target = if is_group { chat_id } else { from };
                            let reply = tokio::select! {
                                result = &mut reply_rx => result,
                                _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                    send_processing(&w_tx, target.clone(), is_group, &w_cfg).await;
                                    reply_rx.await
                                }
                            };
                            if let Ok(r) = reply {
                                let pending = r.pending_analysis;
                                if !r.is_empty {
                                    let _ = w_tx
                                        .send(OutboundMessage {
                                            target_id: target.clone(),
                                            is_group,
                                            text: r.text,
                                            reply_to: None,
                                            images: r.images,
                                            files: r.files,
                                            channel: None,                                        })
                                        .await;
                                }
                                if let Some(analysis) = pending {
                                    handle_pending_analysis(
                                        analysis, Arc::clone(&handle), &w_tx,
                                        target, is_group, &w_cfg,
                                    ).await;
                                }
                            }
                                }
                            ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "wecom: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "wecom: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&from).unwrap().clone()
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
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: target,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
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
                                    let _ = tx.send(reply).await;
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
                                    let _ = tx.send(OutboundMessage {
                                        target_id: target,
                                        is_group,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
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

        let _ = manager.register(Arc::clone(&wecom) as Arc<dyn crate::channel::Channel>);
        let wecom_send = Arc::clone(&wecom);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = wecom_send.send(msg).await {
                    error!("wecom send: {e:#}");
                }
            }
        });

        // First account fills the webhook slot for backward compatibility.
        let _ = wecom_slot.set(Arc::clone(&wecom));

        tokio::spawn(async move {
            if let Err(e) = wecom.run().await {
                error!("wecom channel: {e:#}");
            }
        });

        info!(account = %acct_for_log, "wecom AI Bot WS channel started");
    } // end for wc_accounts
}
