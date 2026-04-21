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

pub(crate) fn start_signal_if_configured(
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
    use crate::channel::signal::SignalChannel;

    let Some(sig_cfg) = &config.channel.channels.signal else {
        return;
    };
    if !sig_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = sig_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = sig_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = sig_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = sig_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("signal", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("signal".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, phone) pairs.
    let mut sig_accounts: Vec<(String, String)> = Vec::new();

    // Legacy: single phone at top level.
    if let Some(p) = &sig_cfg.phone {
        sig_accounts.push(("default".to_owned(), p.clone()));
    }

    // Multi-account: channels.signal.accounts.<name>.phone
    if let Some(accts) = &sig_cfg.accounts {
        for (name, acct) in accts {
            if let Some(p) = acct.get("phone").and_then(|v| v.as_str()) {
                if !sig_accounts.iter().any(|(_, ep)| ep == p) {
                    sig_accounts.push((name.clone(), p.to_owned()));
                }
            }
        }
    }

    if sig_accounts.is_empty() {
        warn!("signal.phone not set, signal disabled");
        return;
    }
    let sig_cli_path = sig_cfg.cli_path.clone();

    for (acct_name, phone) in sig_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let sig_cli_path = sig_cli_path.clone();
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());

        // Per-user inbound queue for Signal.
        type SigItem = (String, String, bool);
        let sig_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<SigItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(move |sender: String, text: String, is_group: bool| {
            let reg = Arc::clone(&reg);
            let cfg = Arc::clone(&cfg_arc);
            let tx = out_tx.clone();
            let enforcer = Arc::clone(&enforcer);
            let group_policy = Arc::clone(&gp);
            let group_allow = Arc::clone(&ga);
            let queues = Arc::clone(&sig_user_queues);
            tokio::spawn(async move {
                // Group policy check.
                if is_group {
                    match group_policy.as_ref() {
                        crate::config::schema::GroupPolicy::Disabled => {
                            debug!("signal group message rejected: groupPolicy=disabled");
                            return;
                        }
                        crate::config::schema::GroupPolicy::Allowlist => {
                            if !group_allow.iter().any(|g| *g == sender) {
                                debug!("signal group message rejected: not in groupAllowFrom");
                                return;
                            }
                        }
                        crate::config::schema::GroupPolicy::Open => {}
                    }
                }
                // DM policy check.
                if !is_group {
                    use crate::channel::PolicyResult;
                    match enforcer.check(&sender).await {
                        PolicyResult::Allow => {}
                        PolicyResult::Deny => {
                            debug!(peer_id = %sender, "signal DM rejected by policy");
                            return;
                        }
                        PolicyResult::SendPairingCode(code) => {
                            if let Err(e) = tx
                                .send(OutboundMessage {
                                    target_id: sender.clone(),
                                    is_group: false,
                                    text: crate::i18n::t_fmt(
                                        "pairing_required",
                                        crate::i18n::default_lang(),
                                        &[("code", &code)],
                                    ),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,

                    files: vec![],                                })
                                .await
                            {
                                tracing::warn!("failed to send message: {e}");
                            }
                            return;
                        }
                        PolicyResult::PairingQueueFull => {
                            if let Err(e) = tx
                                .send(OutboundMessage {
                                    target_id: sender.clone(),
                                    is_group: false,
                                    text: crate::i18n::t(
                                        "pairing_queue_full",
                                        crate::i18n::default_lang(),
                                    )
                                    .to_owned(),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,

                    files: vec![],                                })
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
                    let needs_create = match map.get(&sender) {
                        Some(existing) if !existing.is_closed() => false,
                        Some(_) => {
                            map.remove(&sender);
                            true
                        }
                        None => true,
                    };
                    if needs_create {
                        let (utx, mut urx) = mpsc::channel::<SigItem>(32);
                        map.insert(sender.clone(), utx.clone());
                        let w_reg = Arc::clone(&reg);
                        let w_cfg = Arc::clone(&cfg);
                        let w_tx = tx.clone();
                        let w_uid = sender.clone();
                        tokio::spawn(async move {
                            while let Some((mut text, sender, is_group)) = urx.recv().await {
                                // Debounce: wait briefly then drain queued messages.
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                while let Ok((extra_text, _, _)) = urx.try_recv() {
                                    if !extra_text.is_empty() && !is_fast_preparse(&extra_text) {
                                        text.push('\n');
                                        text.push_str(&extra_text);
                                    }
                                }
                                let process_result = tokio::time::timeout(
                                Duration::from_secs(600),
                                async {
                            let handle = match w_reg.route("signal") {
                                Ok(h) => h,
                                Err(e) => { error!("signal route: {e:#}"); return; }
                            };
                            let dm_scope = default_dm_scope(&w_cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: sender.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "signal".to_string(),
                                peer_id: sender.clone(),
                                dm_scope,
                            });
                            let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "signal".to_string(),
                                peer_id: sender.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images: vec![],
                                files: vec![],
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            let reply = tokio::select! {
                                result = &mut reply_rx => result,
                                _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                    send_processing(&w_tx, sender.clone(), is_group, &w_cfg).await;
                                    reply_rx.await
                                }
                            };
                            if let Ok(r) = reply {
                                let pending = r.pending_analysis;
                                if !r.is_empty {
                                    if let Err(e) = w_tx
                                        .send(OutboundMessage {
                                            target_id: sender.clone(),
                                            is_group,
                                            text: r.text,
                                            reply_to: None,
                                            images: r.images,
                                            files: r.files,
                                            channel: None,                                        })
                                        .await
                                    {
                                        tracing::warn!("failed to send message: {e}");
                                    }
                                }
                                if let Some(analysis) = pending {
                                    handle_pending_analysis(
                                        analysis, Arc::clone(&handle), &w_tx,
                                        sender, is_group, &w_cfg,
                                    ).await;
                                }
                            }
                                }
                            ).await;
                                if process_result.is_err() {
                                    warn!(user = %w_uid, "signal: message processing timed out (600s), skipping to next");
                                }
                            }
                            debug!(user = %w_uid, "signal: per-user worker stopped");
                        });
                        utx
                    } else {
                        map.get(&sender).expect("queue entry must exist").clone()
                    }
                };
                // /btw bypass: spawn directly, skip the per-user queue
                if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                    let reg = Arc::clone(&reg);
                    let tx = tx.clone();
                    let cfg = Arc::clone(&cfg);
                    let question = text[5..].to_owned();
                    let sender = sender.clone();
                    tokio::spawn(async move {
                        let handle = match reg.route("signal") {
                            Ok(h) => h,
                            Err(_) => return,
                        };
                        if let Some(reply_text) =
                            btw_direct_call(&question, &handle.live_status, &handle.providers, &cfg)
                                .await
                        {
                            if let Err(e) = tx
                                .send(OutboundMessage {
                                    target_id: sender,
                                    is_group: false,
                                    text: format!("[/btw] {}", reply_text),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,

                    files: vec![],                                })
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
                    let sender = sender.clone();
                    tokio::spawn(async move {
                        let handle = match reg.route("signal") {
                            Ok(h) => h,
                            Err(_) => return,
                        };
                        let dm_scope = default_dm_scope(&cfg);
                        let session_key = derive_session_key(&SessionKeyParams {
                            agent_id: handle.id.clone(),
                            kind: if is_group {
                                MessageKind::GroupMessage {
                                    group_id: sender.clone(),
                                    thread_id: None,
                                }
                            } else {
                                MessageKind::DirectMessage { account_id: None }
                            },
                            channel: "signal".to_string(),
                            peer_id: sender.clone(),
                            dm_scope,
                        });
                        if let Some(mut reply) = try_preparse_locally(&text, &handle).await {
                            reply.target_id = sender.clone();
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
                            channel: "signal".to_string(),
                            peer_id: sender.clone(),
                            chat_id: String::new(),
                            reply_tx,
                            extra_tools: vec![],
                            images: vec![],
                            files: vec![],
                        };
                        if handle.tx.send(msg).await.is_err() {
                            return;
                        }
                        if let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await {
                            if !r.is_empty {
                                if let Err(e) = tx.send(OutboundMessage {
                                    target_id: sender,
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
                if let Err(e) = user_tx.try_send((text, sender.clone(), is_group)) {
                    warn!(user = %sender, error = %e, "signal: user queue full, dropping message");
                }
            });
        });

        // spawn() is async — drive it in a task.
        tokio::spawn(async move {
            match SignalChannel::spawn(phone, sig_cli_path, on_message).await {
                Ok(ch) => {
                    let ch = Arc::new(ch);
                    let ch_send = Arc::clone(&ch);
                    tokio::spawn(async move {
                        while let Some(msg) = out_rx.recv().await {
                            if let Err(e) = ch_send.send(msg).await {
                                error!("signal send: {e:#}");
                            }
                        }
                    });
                    info!(account = %acct_for_log, "signal channel started");
                    if let Err(e) = ch.run().await {
                        error!("signal channel: {e:#}");
                    }
                }
                Err(e) => warn!("signal-cli not available: {e:#}"),
            }
        });

        // Register a placeholder so ChannelManager knows signal is configured.
        // The real channel handle is inside the spawned task above.
        let _ = manager; // manager.register() can't be called here without the real Arc
    } // end for sig_accounts
}
