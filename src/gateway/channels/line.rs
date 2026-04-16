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

pub(crate) fn start_line_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    line_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::line::LineChannel>>>,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    use crate::channel::line::LineChannel;

    let Some(line_cfg) = &config.channel.channels.line else {
        return;
    };
    if !line_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = line_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = line_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = line_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = line_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("line", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("line".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, access_token) pairs.
    let mut line_accounts: Vec<(String, String)> = Vec::new();

    // Legacy: single token at top level.
    if let Some(token) = line_cfg
        .channel_access_token
        .as_ref()
        .and_then(|t| t.as_plain())
        .map(str::to_owned)
        .or_else(|| std::env::var("LINE_CHANNEL_ACCESS_TOKEN").ok())
    {
        line_accounts.push(("default".to_owned(), token));
    }

    // Multi-account: channels.line.accounts.<name>.channelAccessToken
    if let Some(accts) = &line_cfg.accounts {
        for (name, acct) in accts {
            if let Some(t) = acct.get("channelAccessToken").and_then(|v| v.as_str()) {
                if !line_accounts.iter().any(|(_, et)| et == t) {
                    line_accounts.push((name.clone(), t.to_owned()));
                }
            }
        }
    }

    if line_accounts.is_empty() {
        warn!("LINE channel_access_token not set, line disabled");
        return;
    }

    for (acct_name, channel_access_token) in line_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());

        // Per-user inbound queue for LINE.
        type LineItem = (
            String,
            String,
            bool,
            Vec<crate::agent::registry::ImageAttachment>,
        );
        let line_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<LineItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |user_id: String,
                  text: String,
                  is_group: bool,
                  images: Vec<crate::agent::registry::ImageAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&line_user_queues);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("line group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == user_id) {
                                    debug!("line group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            crate::config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_group {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&user_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %user_id, "line DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: user_id.clone(),
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
                                    .await;
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: user_id.clone(),
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
                                    .await;
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&user_id) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&user_id);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<LineItem>(32);
                            map.insert(user_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_tx = tx.clone();
                            let w_uid = user_id.clone();
                            tokio::spawn(async move {
                                while let Some((mut text, user_id, is_group, mut images)) = urx.recv().await
                                {
                                    // Debounce: wait briefly then drain queued messages.
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                    while let Ok((extra_text, _, _, extra_images)) = urx.try_recv() {
                                        if !extra_text.is_empty() && !is_fast_preparse(&extra_text) {
                                            text.push('\n');
                                            text.push_str(&extra_text);
                                        }
                                        images.extend(extra_images);
                                    }
                                    let process_result = tokio::time::timeout(
                                    Duration::from_secs(600),
                                    async {
                                let handle = match w_reg.route("line") {
                                    Ok(h) => h,
                                    Err(e) => { error!("line route: {e:#}"); return; }
                                };
                                let dm_scope = default_dm_scope(&w_cfg);
                                let session_key = derive_session_key(&SessionKeyParams {
                                    agent_id: handle.id.clone(),
                                    kind: if is_group {
                                        MessageKind::DirectMessage { account_id: None }
                                    } else {
                                        MessageKind::DirectMessage { account_id: None }
                                    },
                                    channel: "line".to_string(),
                                    peer_id: user_id.clone(),
                                    dm_scope,
                                });
                                let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                let msg = AgentMessage {
                                    session_key,
                                    text,
                                    channel: "line".to_string(),
                                    peer_id: user_id.clone(),
                                    chat_id: String::new(),
                                    reply_tx,
                                    extra_tools: vec![],
                                    images,
                                    files: vec![],
                                };
                                if handle.tx.send(msg).await.is_err() {
                                    return;
                                }
                                let reply = tokio::select! {
                                    result = &mut reply_rx => result,
                                    _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                        send_processing(&w_tx, user_id.clone(), is_group, &w_cfg).await;
                                        reply_rx.await
                                    }
                                };
                                if let Ok(r) = reply {
                                    let pending = r.pending_analysis;
                                    if !r.is_empty {
                                        let _ = w_tx
                                            .send(OutboundMessage {
                                                target_id: user_id.clone(),
                                                is_group,
                                                text: r.text,
                                                reply_to: None,
                                                images: r.images,
                                                files: r.files,
                                                channel: None,                                            })
                                            .await;
                                    }
                                    if let Some(analysis) = pending {
                                        handle_pending_analysis(
                                            analysis, Arc::clone(&handle), &w_tx,
                                            user_id, is_group, &w_cfg,
                                        ).await;
                                    }
                                }
                                    }
                                ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "line: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "line: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&user_id).unwrap().clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let user_id = user_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("line") {
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
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: user_id,
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
                        let user_id = user_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("line") {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: user_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "line".to_string(),
                                peer_id: user_id.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(&text, &handle).await {
                                reply.target_id = user_id.clone();
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
                                channel: "line".to_string(),
                                peer_id: user_id.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files: vec![],
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await {
                                if !r.is_empty {
                                    let _ = tx.send(OutboundMessage {
                                        target_id: user_id,
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
                    if let Err(e) = user_tx.try_send((text, user_id.clone(), is_group, images)) {
                        warn!(user = %user_id, error = %e, "line: user queue full, dropping message");
                    }
                });
            },
        );

        let line = Arc::new(LineChannel::with_api_base(
            channel_access_token,
            line_cfg.api_base.clone(),
            on_message,
        ));
        let _ = line_slot.set(Arc::clone(&line));
        let line_send = Arc::clone(&line);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = line_send.send(msg).await {
                    error!("line send: {e:#}");
                }
            }
        });
        let _ = manager.register(Arc::clone(&line) as Arc<dyn Channel>);
        tokio::spawn(async move {
            if let Err(e) = line.run().await {
                error!("line channel: {e:#}");
            }
        });
        info!(account = %acct_for_log, "line channel started (webhook mode)");
    } // end for line_accounts
}
