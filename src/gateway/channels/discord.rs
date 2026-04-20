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

pub(crate) fn start_discord_if_configured(
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
) {
    use crate::channel::discord::DiscordChannel;

    let Some(dc_cfg) = &config.channel.channels.discord else {
        return;
    };
    if !dc_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Collect (account_name, token) pairs.
    let mut dc_accounts: Vec<(String, String)> = Vec::new();

    // Legacy: single token at top level.
    if let Some(token) = dc_cfg.token.as_ref().and_then(|t| t.as_plain()) {
        dc_accounts.push(("default".to_owned(), token.to_owned()));
    }

    // Multi-account: channels.discord.accounts.<name>.token
    if let Some(accts) = &dc_cfg.accounts {
        for (name, acct) in accts {
            if let Some(t) = acct.get("token").and_then(|v| v.as_str()) {
                if !dc_accounts.iter().any(|(_, existing)| existing == t) {
                    dc_accounts.push((name.clone(), t.to_owned()));
                }
            }
        }
    }

    if dc_accounts.is_empty() {
        warn!("discord token not set");
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = dc_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = dc_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = dc_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = dc_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("discord", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("discord".to_owned(), Arc::clone(&enforcer));
    }

    let allow_bots = dc_cfg.allow_bots.unwrap_or(false);

    for (acct_name, token) in dc_accounts {
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register Discord channel sender for notification routing.
        {
            let mut senders = channel_senders.write().expect("channel_senders lock poisoned");
            senders.insert(format!("discord/{}", acct_name), out_tx.clone());
        }

        // Find binding for this account.
        let bound_agent = config
            .agents
            .bindings
            .iter()
            .find(|b| {
                b.match_.channel.as_deref() == Some("discord")
                    && b.match_.account_id.as_deref() == Some(&acct_name)
            })
            .map(|b| b.agent_id.clone());
        let bound = bound_agent.clone();

        // Per-user inbound queue for Discord.
        type DcItem = (String, String, String, bool, Option<String>);
        let dc_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<DcItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |peer_id: String, text: String, channel_id: String, is_guild: bool| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let bound = bound.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&dc_user_queues);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_guild {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("discord group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == channel_id) {
                                    debug!("discord group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            crate::config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_guild {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&peer_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %peer_id, "discord DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: channel_id.clone(),
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
                                        target_id: channel_id.clone(),
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
                        let needs_create = match map.get(&peer_id) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&peer_id);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<DcItem>(32);
                            map.insert(peer_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_tx = tx.clone();
                            let w_uid = peer_id.clone();
                            tokio::spawn(async move {
                                while let Some((mut text, peer_id, channel_id, is_guild, bound)) =
                                    urx.recv().await
                                {
                                    // Debounce: wait briefly then drain queued messages.
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                    while let Ok((extra_text, _, _, _, _)) = urx.try_recv() {
                                        if !extra_text.is_empty() && !is_fast_preparse(&extra_text) {
                                            text.push('\n');
                                            text.push_str(&extra_text);
                                        }
                                    }
                                    let process_result = tokio::time::timeout(
                                        Duration::from_secs(172800), // 48 hours, matching OpenClaw default
                                        async {
                                    let handle = if let Some(ref agent_id) = bound {
                                        match w_reg.get(agent_id) {
                                            Ok(h) => h,
                                            Err(_) => match w_reg.route("discord") {
                                                Ok(h) => h,
                                                Err(e) => { error!("discord route: {e:#}"); return; }
                                            },
                                        }
                                    } else {
                                        match w_reg.route("discord") {
                                            Ok(h) => h,
                                            Err(e) => { error!("discord route: {e:#}"); return; }
                                        }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: if is_guild {
                                            MessageKind::GroupMessage {
                                                group_id: channel_id.clone(),
                                                thread_id: None,
                                            }
                                        } else {
                                            MessageKind::DirectMessage { account_id: None }
                                        },
                                        channel: "discord".to_string(),
                                        peer_id: peer_id.clone(),
                                        dm_scope,
                                    });
                                    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                    let msg = AgentMessage {
                                        session_key,
                                        text,
                                        channel: "discord".to_string(),
                                        peer_id,
                                        chat_id: String::new(),
                                        reply_tx,
                                        extra_tools: vec![],
                                        images: vec![],
                                        files: vec![],
                                        is_internal: false,
                                    };
                                    if handle.tx.send(msg).await.is_err() {
                                        return;
                                    }
                                    let reply = tokio::select! {
                                        result = &mut reply_rx => result,
                                        _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                            send_processing(&w_tx, channel_id.clone(), is_guild, &w_cfg).await;
                                            reply_rx.await
                                        }
                                    };
                                    if let Ok(r) = reply {
                                        let pending = r.pending_analysis;
                                        if !r.is_empty {
                                            if let Err(e) = w_tx
                                                .send(OutboundMessage {
                                                    target_id: channel_id.clone(),
                                                    is_group: is_guild,
                                                    text: r.text,
                                                    reply_to: None,
                                                    images: r.images,
                                                    files: r.files,
                                                    channel: None,                                                })
                                                .await
                                            {
                                                tracing::warn!("failed to send message: {e}");
                                            }
                                        }
                                        if let Some(analysis) = pending {
                                            handle_pending_analysis(
                                                analysis, Arc::clone(&handle), &w_tx,
                                                channel_id, is_guild, &w_cfg,
                                            ).await;
                                        }
                                    }
                                        }
                                    ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "discord: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "discord: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&peer_id).expect("queue entry must exist").clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let channel_id = channel_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("discord") {
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
                                        target_id: channel_id,
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
                        let peer_id = peer_id.clone();
                        let channel_id = channel_id.clone();
                        let bound = bound.clone();
                        tokio::spawn(async move {
                            let handle = if let Some(ref agent_id) = bound {
                                match reg.get(agent_id) {
                                    Ok(h) => h,
                                    Err(_) => match reg.route("discord") {
                                        Ok(h) => h,
                                        Err(_) => return,
                                    },
                                }
                            } else {
                                match reg.route("discord") {
                                    Ok(h) => h,
                                    Err(_) => return,
                                }
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_guild {
                                    MessageKind::GroupMessage {
                                        group_id: channel_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "discord".to_string(),
                                peer_id: peer_id.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(&text, &handle).await {
                                reply.target_id = channel_id;
                                reply.is_group = is_guild;
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
                                channel: "discord".to_string(),
                                peer_id,
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images: vec![],
                                files: vec![],
                                is_internal: false,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await {
                                if !r.is_empty {
                                    if let Err(e) = tx.send(OutboundMessage {
                                        target_id: channel_id,
                                        is_group: is_guild,
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
                        user_tx.try_send((text, peer_id.clone(), channel_id, is_guild, bound))
                    {
                        warn!(user = %peer_id, error = %e, "discord: user queue full, dropping message");
                    }
                });
            },
        );

        let dc = Arc::new(DiscordChannel::new(
            token,
            allow_bots,
            on_message,
            dc_cfg.api_base.clone(),
            dc_cfg.gateway_url.clone(),
        ));
        let dc_send = Arc::clone(&dc);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = dc_send.send(msg).await {
                    error!("discord send: {e:#}");
                }
            }
        });
        if let Err(e) = manager.register(Arc::clone(&dc) as Arc<dyn Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        tokio::spawn(async move {
            if let Err(e) = dc.run().await {
                error!("discord channel: {e:#}");
            }
        });
        info!(account = %acct_for_log, "discord channel started");
    }
}
