use std::{sync::Arc, time::Duration};

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::{
    agent::{AgentMessage, AgentRegistry},
    channel::{Channel, OutboundMessage},
    config::runtime::RuntimeConfig,
    gateway::session::{MessageKind, SessionKeyParams, derive_session_key},
};

use super::super::startup::handle_pending_analysis;
use super::default_dm_scope;

// ---------------------------------------------------------------------------
// Custom channels (webhook + websocket)
// ---------------------------------------------------------------------------

pub(crate) fn start_custom_channels(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    custom_webhooks: Arc<
        std::sync::RwLock<
            std::collections::HashMap<String, Arc<crate::channel::custom::CustomWebhookChannel>>,
        >,
    >,
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
                );
            }
            "websocket" => {
                start_custom_websocket(config, ch_cfg.clone(), Arc::clone(&registry), manager);
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
    ch_cfg: crate::config::schema::CustomChannelConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    custom_webhooks: Arc<
        std::sync::RwLock<
            std::collections::HashMap<String, Arc<crate::channel::custom::CustomWebhookChannel>>,
        >,
    >,
) {
    use crate::channel::custom::CustomWebhookChannel;

    let ch_name = ch_cfg.name.clone();
    let reg = Arc::clone(&registry);
    let cfg_arc = Arc::new(config.clone());
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

    let ch_name_cb = ch_name.clone();
    let on_message = Arc::new(move |sender: String, text: String, is_group: bool| {
        let reg = Arc::clone(&reg);
        let cfg = Arc::clone(&cfg_arc);
        let tx = out_tx.clone();
        let ch_name = ch_name_cb.clone();
        tokio::spawn(async move {
            let handle = match reg.route(&ch_name) {
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
                        group_id: sender.clone(),
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
                chat_id: sender.clone(),
                reply_tx,
                extra_tools: vec![],
                images: vec![],
                files: vec![],
            };
            if handle.tx.send(msg).await.is_err() {
                return;
            }
            if let Ok(Ok(r)) = tokio::time::timeout(Duration::from_secs(120), reply_rx).await {
                let pending = r.pending_analysis;
                if !r.is_empty {
                    if let Err(e) = tx
                        .send(OutboundMessage {
                            target_id: sender.clone(),
                            is_group,
                            text: r.text,
                            reply_to: None,
                            images: r.images,
                            files: r.files,
                            channel: None,                        })
                        .await
                    {
                        tracing::warn!("failed to send message: {e}");
                    }
                }
                if let Some(analysis) = pending {
                    handle_pending_analysis(
                        analysis,
                        Arc::clone(&handle),
                        &tx,
                        sender,
                        is_group,
                        &cfg,
                    )
                    .await;
                }
            }
        });
    });

    let ch = Arc::new(CustomWebhookChannel::new(ch_cfg, on_message));

    // Register in the custom_webhooks map for /hooks/{name} dispatch.
    if let Ok(mut map) = custom_webhooks.write() {
        map.insert(ch_name.clone(), Arc::clone(&ch));
    }

    let ch_send = Arc::clone(&ch);
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Err(e) = ch_send.send(msg).await {
                error!(channel = %ch_send.cfg.name, "custom webhook send error: {e:#}");
            }
        }
    });

    if let Err(e) = manager.register(Arc::clone(&ch) as Arc<dyn Channel>) {
        tracing::warn!("failed to register channel: {e}");
    }
    tokio::spawn(async move {
        if let Err(e) = ch.run().await {
            error!("custom webhook channel error: {e:#}");
        }
    });
    info!(channel = %ch_name, "custom webhook channel started");
}

fn start_custom_websocket(
    config: &RuntimeConfig,
    ch_cfg: crate::config::schema::CustomChannelConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
) {
    use crate::channel::custom::CustomWebSocketChannel;

    let ch_name = ch_cfg.name.clone();
    let reg = Arc::clone(&registry);
    let cfg_arc = Arc::new(config.clone());
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

    let ch_name_cb = ch_name.clone();
    let on_message = Arc::new(move |sender: String, text: String, is_group: bool| {
        let reg = Arc::clone(&reg);
        let cfg = Arc::clone(&cfg_arc);
        let tx = out_tx.clone();
        let ch_name = ch_name_cb.clone();
        tokio::spawn(async move {
            let handle = match reg.route(&ch_name) {
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
                        group_id: sender.clone(),
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
                chat_id: sender.clone(),
                reply_tx,
                extra_tools: vec![],
                images: vec![],
                files: vec![],
            };
            if handle.tx.send(msg).await.is_err() {
                return;
            }
            if let Ok(Ok(r)) = tokio::time::timeout(Duration::from_secs(120), reply_rx).await {
                let pending = r.pending_analysis;
                if !r.is_empty {
                    if let Err(e) = tx
                        .send(OutboundMessage {
                            target_id: sender.clone(),
                            is_group,
                            text: r.text,
                            reply_to: None,
                            images: r.images,
                            files: r.files,
                            channel: None,                        })
                        .await
                    {
                        tracing::warn!("failed to send message: {e}");
                    }
                }
                if let Some(analysis) = pending {
                    handle_pending_analysis(
                        analysis,
                        Arc::clone(&handle),
                        &tx,
                        sender,
                        is_group,
                        &cfg,
                    )
                    .await;
                }
            }
        });
    });

    let ch = Arc::new(CustomWebSocketChannel::new(ch_cfg, on_message));

    let ch_send = Arc::clone(&ch);
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Err(e) = ch_send.send(msg).await {
                error!(channel = %ch_send.cfg.name, "custom WS send error: {e:#}");
            }
        }
    });

    if let Err(e) = manager.register(Arc::clone(&ch) as Arc<dyn Channel>) {
        tracing::warn!("failed to register channel: {e}");
    }
    tokio::spawn(async move {
        if let Err(e) = ch.run().await {
            error!("custom WS channel error: {e:#}");
        }
    });
    info!(channel = %ch_name, "custom websocket channel started");
}
