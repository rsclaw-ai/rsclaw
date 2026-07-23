use std::{sync::Arc, time::Duration};

use rsclaw_agent::{AgentMessage, AgentRegistry};
use rsclaw_channel::{Channel, DmPolicyEnforcer, OutboundMessage, PolicyResult};
use rsclaw_config::runtime::RuntimeConfig;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use super::{
    super::preparse::{btw_direct_call, is_fast_preparse, try_preparse_locally},
    default_dm_scope,
};
use crate::gateway::session::{MessageKind, SessionKeyParams, derive_session_key};

// ---------------------------------------------------------------------------
// Custom channels (webhook + websocket)
// ---------------------------------------------------------------------------

pub(crate) fn start_custom_channels(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &rsclaw_channel::ChannelManager,
    custom_webhooks: Arc<
        std::sync::RwLock<
            std::collections::HashMap<String, Arc<rsclaw_channel::custom::CustomWebhookChannel>>,
        >,
    >,
    channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
    redb_store: Arc<rsclaw_store::redb_store::RedbStore>,
    shutdown: crate::gateway::ShutdownCoordinator,
) {
    let custom_cfgs = match &config.channel.channels.custom {
        Some(cfgs) => cfgs,
        None => return,
    };

    for ch_cfg in custom_cfgs {
        if !ch_cfg.base.enabled.unwrap_or(true) {
            continue;
        }

        let ch_name = ch_cfg.name.clone();

        match ch_cfg.channel_type.as_str() {
            "webhook" => {
                start_custom_webhook(
                    config,
                    ch_cfg.clone(),
                    Arc::clone(&registry),
                    manager,
                    Arc::clone(&custom_webhooks),
                    Arc::clone(&channel_senders),
                    Arc::clone(&redb_store),
                    shutdown.clone(),
                );
            }
            "websocket" => {
                start_custom_websocket(
                    config,
                    ch_cfg.clone(),
                    Arc::clone(&registry),
                    manager,
                    Arc::clone(&channel_senders),
                    Arc::clone(&redb_store),
                    shutdown.clone(),
                );
            }
            other => {
                warn!(
                    channel = %ch_name,
                    channel_type = %other,
                    "unknown custom channel type, skipping"
                );
            }
        }
    }
}

fn start_custom_webhook(
    config: &RuntimeConfig,
    ch_cfg: rsclaw_config::schema::CustomChannelConfig,
    registry: Arc<AgentRegistry>,
    manager: &rsclaw_channel::ChannelManager,
    custom_webhooks: Arc<
        std::sync::RwLock<
            std::collections::HashMap<String, Arc<rsclaw_channel::custom::CustomWebhookChannel>>,
        >,
    >,
    channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
    redb_store: Arc<rsclaw_store::redb_store::RedbStore>,
    shutdown: crate::gateway::ShutdownCoordinator,
) {
    use rsclaw_channel::custom::CustomWebhookChannel;

    let ch_name = ch_cfg.name.clone();

    // DM policy enforcer.
    let dm_policy = ch_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(rsclaw_config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = ch_cfg.base.allow_from.clone().unwrap_or_default();
    let enforcer = Arc::new(
        DmPolicyEnforcer::new(dm_policy, allow_from).with_persistence(&ch_name, redb_store),
    );

    let reg = Arc::clone(&registry);
    let cfg_arc = Arc::new(config.clone());
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

    // Register the channel sender so cron / watch can dispatch messages here.
    {
        let mut senders = channel_senders
            .write()
            .expect("channel_senders lock poisoned");
        senders.insert(ch_name.clone(), out_tx.clone());
    }

    let ch_name_cb = ch_name.clone();
    let on_message = Arc::new(
        move |sender: String,
              text: String,
              chat_id: String,
              images: Vec<rsclaw_agent::registry::ImageAttachment>,
              files: Vec<rsclaw_agent::registry::FileAttachment>,
              is_group: bool| {
            let reg = Arc::clone(&reg);
            let cfg = Arc::clone(&cfg_arc);
            let tx = out_tx.clone();
            let ch_name = ch_name_cb.clone();
            let enforcer = Arc::clone(&enforcer);
            // Where agent replies go: groups → group chat, DMs → speaker.
            let reply_target = if is_group {
                chat_id.clone()
            } else {
                sender.clone()
            };
            tokio::spawn(async move {
                // DM policy check (skip for groups — group_allow_from would
                // belong here once the schema exposes it for custom channels).
                if !is_group {
                    match enforcer.check(&sender).await {
                        PolicyResult::Allow => {}
                        PolicyResult::Deny => {
                            warn!(peer_id = %sender, "custom channel DM rejected by policy");
                            return;
                        }
                        PolicyResult::SendPairingCode(code) => {
                            if let Err(e) = tx
                                .send(OutboundMessage {
                                    target_id: sender.clone(),
                                    is_group: false,
                                    text: rsclaw_i18n::t_fmt(
                                        "pairing_required",
                                        rsclaw_i18n::default_lang(),
                                        &[("code", &code)],
                                    )
                                    .to_owned(),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,
                                    account: None,
                                    files: vec![],
                                })
                                .await
                            {
                                tracing::warn!("failed to send pairing message: {e}");
                            }
                            return;
                        }
                        PolicyResult::PairingQueueFull => {
                            if let Err(e) = tx
                                .send(OutboundMessage {
                                    target_id: sender.clone(),
                                    is_group: false,
                                    text: rsclaw_i18n::t(
                                        "pairing_queue_full",
                                        rsclaw_i18n::default_lang(),
                                    )
                                    .to_owned(),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,
                                    account: None,
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

                // /btw bypass
                if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                    let reg = Arc::clone(&reg);
                    let tx = tx.clone();
                    let cfg = cfg.clone();
                    let question = text[5..].to_owned();
                    let reply_target = reply_target.clone();
                    tokio::spawn(async move {
                        let handle = match reg.route_account(&ch_name, None) {
                            Ok(h) => h,
                            Err(_) => return,
                        };
                        if let Some(reply_text) =
                            btw_direct_call(&question, &handle.live_status, &handle.providers, &cfg)
                                .await
                        {
                            if let Err(e) = tx
                                .send(OutboundMessage {
                                    target_id: reply_target,
                                    is_group,
                                    text: format!("[/btw] {}", reply_text),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,
                                    account: None,
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

                // Fast preparse bypass
                if is_fast_preparse(&text) {
                    let reg = Arc::clone(&reg);
                    let tx = tx.clone();
                    let cfg = cfg.clone();
                    let sender = sender.clone();
                    let chat_id_inner = chat_id.clone();
                    let reply_target = reply_target.clone();
                    tokio::spawn(async move {
                        let handle = match reg.route_account(&ch_name, None) {
                            Ok(h) => h,
                            Err(_) => return,
                        };
                        let dm_scope = default_dm_scope(&cfg);
                        let session_key = derive_session_key(&SessionKeyParams {
                            agent_id: handle.id.clone(),
                            kind: if is_group {
                                MessageKind::GroupMessage {
                                    group_id: chat_id_inner.clone(),
                                    thread_id: None,
                                }
                            } else {
                                MessageKind::DirectMessage { account_id: None }
                            },
                            channel: ch_name.clone(),
                            peer_id: sender.clone(),
                            dm_scope,
                        });
                        if let Some(mut reply) = try_preparse_locally(
                            &text,
                            &handle,
                            &ch_name,
                            &sender,
                            crate::gateway::preparse::PreparseOrigin::User,
                        )
                        .await
                        {
                            reply.target_id = reply_target.clone();
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
                            channel: ch_name.clone(),
                            peer_id: sender.clone(),
                            chat_id: chat_id_inner.clone(),
                            reply_tx,
                            task_id: None,
                            context_id: None,
                            event_tx: None,
                            cancel_token: None,
                            input_request_tx: None,
                            extra_tools: vec![],
                            images,
                            files,
                            account: None,
                        };
                        if handle.tx.send(msg).await.is_err() {
                            return;
                        }
                        if let Ok(Ok(r)) =
                            tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await
                        {
                            if !r.is_empty {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: reply_target,
                                        is_group,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                        account: None,
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

                // Normal dispatch
                let handle = match reg.route_account(&ch_name, None) {
                    Ok(h) => h,
                    Err(e) => {
                        error!(channel = %ch_name, "route error: {e:#}");
                        return;
                    }
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
                    channel: ch_name.clone(),
                    peer_id: sender.clone(),
                    dm_scope,
                });
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let msg = AgentMessage {
                    session_key,
                    text,
                    channel: ch_name.clone(),
                    peer_id: sender.clone(),
                    chat_id: chat_id.clone(),
                    reply_tx,
                    task_id: None,
                    context_id: None,
                    event_tx: None,
                    cancel_token: None,
                    input_request_tx: None,
                    extra_tools: vec![],
                    images,
                    files,
                    account: None,
                };
                if handle.tx.send(msg).await.is_err() {
                    return;
                }
                if let Ok(Ok(r)) = tokio::time::timeout(Duration::from_secs(120), reply_rx).await {
                    let pending = r.pending_analysis;
                    if !r.is_empty {
                        if let Err(e) = tx
                            .send(OutboundMessage {
                                target_id: reply_target.clone(),
                                is_group,
                                text: r.text,
                                reply_to: None,
                                images: r.images,
                                files: r.files,
                                account: None,
                                channel: None,
                            })
                            .await
                        {
                            tracing::warn!("failed to send message: {e}");
                        }
                    }
                    if let Some(analysis) = pending {
                        crate::gateway::startup::handle_pending_analysis(
                            analysis,
                            Arc::clone(&handle),
                            &tx,
                            reply_target,
                            is_group,
                            &cfg,
                        )
                        .await;
                    }
                }
            });
        },
    );

    let ch = Arc::new(CustomWebhookChannel::new(ch_cfg, on_message));

    // Register in the custom_webhooks map for /hooks/{name} dispatch.
    if let Ok(mut map) = custom_webhooks.write() {
        map.insert(ch_name.clone(), Arc::clone(&ch));
    }

    let ch_send = Arc::clone(&ch);
    let shutdown_for_out = shutdown.clone();
    let chan_name = ch.name().to_owned();
    let cancel_token = manager.register_cancel_token(&chan_name);
    let cancel_for_out = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = shutdown_for_out.notified() => {
                    info!("custom: drain signaled, stopping outbound sender");
                    break;
                }
                () = cancel_for_out.cancelled() => {
                    info!("custom: channel cancelled, stopping outbound sender");
                    break;
                }
                msg = out_rx.recv() => {
                    let Some(msg) = msg else { break };
                    if let Err(e) = ch_send.send(msg).await {
                        error!(channel = %ch_send.cfg.name, "custom webhook send error: {e:#}");
                    }
                }
            }
        }
    });

    if let Err(e) = manager.register(Arc::clone(&ch) as Arc<dyn Channel>) {
        tracing::warn!("failed to register channel: {e}");
    }
    let shutdown_for_run = shutdown.clone();
    tokio::spawn(async move {
        tokio::select! {
            res = ch.run() => {
                if let Err(e) = res {
                    error!("custom webhook channel error: {e:#}");
                }
            }
            () = shutdown_for_run.notified() => {
                info!("custom: drain signaled, stopping run loop");
            }
            () = cancel_token.cancelled() => {
                info!("custom: channel cancelled, stopping run loop");
            }
        }
    });
    info!(channel = %ch_name, "custom webhook channel started");
}

fn start_custom_websocket(
    config: &RuntimeConfig,
    ch_cfg: rsclaw_config::schema::CustomChannelConfig,
    registry: Arc<AgentRegistry>,
    manager: &rsclaw_channel::ChannelManager,
    channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
    redb_store: Arc<rsclaw_store::redb_store::RedbStore>,
    shutdown: crate::gateway::ShutdownCoordinator,
) {
    use rsclaw_channel::custom::CustomWebSocketChannel;

    let ch_name = ch_cfg.name.clone();

    // DM policy enforcer.
    let dm_policy = ch_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(rsclaw_config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = ch_cfg.base.allow_from.clone().unwrap_or_default();
    let enforcer = Arc::new(
        DmPolicyEnforcer::new(dm_policy, allow_from).with_persistence(&ch_name, redb_store),
    );

    let reg = Arc::clone(&registry);
    let cfg_arc = Arc::new(config.clone());
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

    // Register the channel sender so cron / watch can dispatch messages here.
    {
        let mut senders = channel_senders
            .write()
            .expect("channel_senders lock poisoned");
        senders.insert(ch_name.clone(), out_tx.clone());
    }

    let ch_name_cb = ch_name.clone();
    let on_message = Arc::new(
        move |sender: String,
              text: String,
              chat_id: String,
              images: Vec<rsclaw_agent::registry::ImageAttachment>,
              files: Vec<rsclaw_agent::registry::FileAttachment>,
              is_group: bool| {
            let reg = Arc::clone(&reg);
            let cfg = Arc::clone(&cfg_arc);
            let tx = out_tx.clone();
            let ch_name = ch_name_cb.clone();
            let enforcer = Arc::clone(&enforcer);
            let reply_target = if is_group {
                chat_id.clone()
            } else {
                sender.clone()
            };
            tokio::spawn(async move {
                if !is_group {
                    match enforcer.check(&sender).await {
                        PolicyResult::Allow => {}
                        PolicyResult::Deny => {
                            warn!(peer_id = %sender, "custom channel DM rejected by policy");
                            return;
                        }
                        PolicyResult::SendPairingCode(code) => {
                            if let Err(e) = tx
                                .send(OutboundMessage {
                                    target_id: sender.clone(),
                                    is_group: false,
                                    text: rsclaw_i18n::t_fmt(
                                        "pairing_required",
                                        rsclaw_i18n::default_lang(),
                                        &[("code", &code)],
                                    )
                                    .to_owned(),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,
                                    account: None,
                                    files: vec![],
                                })
                                .await
                            {
                                tracing::warn!("failed to send pairing message: {e}");
                            }
                            return;
                        }
                        PolicyResult::PairingQueueFull => {
                            if let Err(e) = tx
                                .send(OutboundMessage {
                                    target_id: sender.clone(),
                                    is_group: false,
                                    text: rsclaw_i18n::t(
                                        "pairing_queue_full",
                                        rsclaw_i18n::default_lang(),
                                    )
                                    .to_owned(),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,
                                    account: None,
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

                // /btw bypass
                if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                    let reg = Arc::clone(&reg);
                    let tx = tx.clone();
                    let cfg = cfg.clone();
                    let question = text[5..].to_owned();
                    let reply_target = reply_target.clone();
                    tokio::spawn(async move {
                        let handle = match reg.route_account(&ch_name, None) {
                            Ok(h) => h,
                            Err(_) => return,
                        };
                        if let Some(reply_text) =
                            btw_direct_call(&question, &handle.live_status, &handle.providers, &cfg)
                                .await
                        {
                            if let Err(e) = tx
                                .send(OutboundMessage {
                                    target_id: reply_target,
                                    is_group,
                                    text: format!("[/btw] {}", reply_text),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,
                                    account: None,
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

                // Fast preparse bypass
                if is_fast_preparse(&text) {
                    let reg = Arc::clone(&reg);
                    let tx = tx.clone();
                    let cfg = cfg.clone();
                    let sender = sender.clone();
                    let chat_id_inner = chat_id.clone();
                    let reply_target = reply_target.clone();
                    tokio::spawn(async move {
                        let handle = match reg.route_account(&ch_name, None) {
                            Ok(h) => h,
                            Err(_) => return,
                        };
                        let dm_scope = default_dm_scope(&cfg);
                        let session_key = derive_session_key(&SessionKeyParams {
                            agent_id: handle.id.clone(),
                            kind: if is_group {
                                MessageKind::GroupMessage {
                                    group_id: chat_id_inner.clone(),
                                    thread_id: None,
                                }
                            } else {
                                MessageKind::DirectMessage { account_id: None }
                            },
                            channel: ch_name.clone(),
                            peer_id: sender.clone(),
                            dm_scope,
                        });
                        if let Some(mut reply) = try_preparse_locally(
                            &text,
                            &handle,
                            &ch_name,
                            &sender,
                            crate::gateway::preparse::PreparseOrigin::User,
                        )
                        .await
                        {
                            reply.target_id = reply_target.clone();
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
                            channel: ch_name.clone(),
                            peer_id: sender.clone(),
                            chat_id: chat_id_inner.clone(),
                            reply_tx,
                            task_id: None,
                            context_id: None,
                            event_tx: None,
                            cancel_token: None,
                            input_request_tx: None,
                            extra_tools: vec![],
                            images,
                            files,
                            account: None,
                        };
                        if handle.tx.send(msg).await.is_err() {
                            return;
                        }
                        if let Ok(Ok(r)) =
                            tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await
                        {
                            if !r.is_empty {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: reply_target,
                                        is_group,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                        account: None,
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

                // Normal dispatch
                let handle = match reg.route_account(&ch_name, None) {
                    Ok(h) => h,
                    Err(e) => {
                        error!(channel = %ch_name, "route error: {e:#}");
                        return;
                    }
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
                    channel: ch_name.clone(),
                    peer_id: sender.clone(),
                    dm_scope,
                });
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let msg = AgentMessage {
                    session_key,
                    text,
                    channel: ch_name.clone(),
                    peer_id: sender.clone(),
                    chat_id: chat_id.clone(),
                    reply_tx,
                    task_id: None,
                    context_id: None,
                    event_tx: None,
                    cancel_token: None,
                    input_request_tx: None,
                    extra_tools: vec![],
                    images,
                    files,
                    account: None,
                };
                if handle.tx.send(msg).await.is_err() {
                    return;
                }
                if let Ok(Ok(r)) = tokio::time::timeout(Duration::from_secs(120), reply_rx).await {
                    let pending = r.pending_analysis;
                    if !r.is_empty {
                        if let Err(e) = tx
                            .send(OutboundMessage {
                                target_id: reply_target.clone(),
                                is_group,
                                text: r.text,
                                reply_to: None,
                                images: r.images,
                                files: r.files,
                                account: None,
                                channel: None,
                            })
                            .await
                        {
                            tracing::warn!("failed to send message: {e}");
                        }
                    }
                    if let Some(analysis) = pending {
                        crate::gateway::startup::handle_pending_analysis(
                            analysis,
                            Arc::clone(&handle),
                            &tx,
                            reply_target,
                            is_group,
                            &cfg,
                        )
                        .await;
                    }
                }
            });
        },
    );

    let ch = Arc::new(CustomWebSocketChannel::new(ch_cfg, on_message));

    let chan_name_for_cancel = ch_name.clone();
    let cancel_token = manager.register_cancel_token(&chan_name_for_cancel);
    let cancel_for_out = cancel_token.clone();

    let ch_send = Arc::clone(&ch);
    let shutdown_for_out = shutdown.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = shutdown_for_out.notified() => {
                    info!("custom WS: drain signaled, stopping outbound sender");
                    break;
                }
                () = cancel_for_out.cancelled() => {
                    info!("custom WS: channel cancelled, stopping outbound sender");
                    break;
                }
                msg = out_rx.recv() => {
                    let Some(msg) = msg else { break };
                    if let Err(e) = ch_send.send(msg).await {
                        error!(channel = %ch_send.cfg.name, "custom WS send error: {e:#}");
                    }
                }
            }
        }
    });

    if let Err(e) = manager.register(Arc::clone(&ch) as Arc<dyn Channel>) {
        tracing::warn!("failed to register channel: {e}");
    }
    let shutdown_for_run = shutdown.clone();
    tokio::spawn(async move {
        tokio::select! {
            res = ch.run() => {
                if let Err(e) = res {
                    error!("custom WS channel error: {e:#}");
                }
            }
            () = shutdown_for_run.notified() => {
                info!("custom WS: drain signaled, stopping run loop");
            }
            () = cancel_token.cancelled() => {
                info!("custom WS: channel cancelled, stopping run loop");
            }
        }
    });
    info!(channel = %ch_name, "custom websocket channel started");
}
