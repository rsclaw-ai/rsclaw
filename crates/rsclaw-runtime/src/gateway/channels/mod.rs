//! Channel construction and startup.
//!
//! Wires each messaging channel (Telegram, Discord, Slack, WeChat, etc.)
//! to agent runtimes with per-user queuing, DM/group policy enforcement,
//! preparse bypass, and `/btw` direct-call support.

mod custom;
mod dingtalk;
mod discord;
mod feishu;
mod line;
mod matrix;
mod qq;
mod signal;
mod slack;
mod telegram;
mod wechat;
mod wecom;
mod whatsapp;
mod zalo;

use std::sync::Arc;

pub(crate) use custom::start_custom_channels;
use rsclaw_agent::{AgentMessage, AgentRegistry};
use rsclaw_channel::{Channel, OutboundMessage, cli::CliChannel, telegram::TelegramChannel};
use rsclaw_config::{runtime::RuntimeConfig, schema::DmScope};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

pub(crate) use self::{
    dingtalk::start_dingtalk_if_configured, discord::start_discord_if_configured,
    feishu::start_feishu_if_configured, line::start_line_if_configured,
    matrix::start_matrix_if_configured, qq::start_qq_if_configured,
    signal::start_signal_if_configured, slack::start_slack_if_configured,
    telegram::start_telegram_if_configured, wechat::start_wechat_personal_if_configured,
    wecom::start_wecom_if_configured, whatsapp::start_whatsapp_if_configured,
    zalo::start_zalo_if_configured,
};
use super::{
    preparse::{btw_direct_call, is_fast_preparse, try_preparse_locally},
    startup::handle_pending_analysis,
};
use crate::gateway::session::{MessageKind, SessionKeyParams, derive_session_key};

pub(crate) fn default_dm_scope(config: &RuntimeConfig) -> DmScope {
    config
        .channel
        .session
        .dm_scope
        .clone()
        .unwrap_or(DmScope::PerChannelPeer)
}

pub(crate) fn start_channels(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &rsclaw_channel::ChannelManager,
    feishu_slot: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::feishu::FeishuChannel>>>,
    wecom_slot: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::wecom::WeComChannel>>>,
    whatsapp_slot: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::whatsapp::WhatsAppChannel>>>,
    line_slot: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::line::LineChannel>>>,
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
    // CLI channel — always started in local mode.
    {
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register CLI channel sender for notification routing.
        {
            let mut senders = channel_senders
                .write()
                .expect("channel_senders lock poisoned");
            senders.insert("cli".to_string(), out_tx.clone());
        }

        let on_message = Arc::new(move |peer_id: String, text: String| {
            let reg = Arc::clone(&reg);
            let cfg = Arc::clone(&cfg_arc);
            let tx = out_tx.clone();
            tokio::spawn(async move {
                let handle = match reg.default_agent() {
                    Ok(h) => h,
                    Err(e) => {
                        error!("no default agent: {e:#}");
                        return;
                    }
                };
                let dm_scope = default_dm_scope(&cfg);
                let session_key = derive_session_key(&SessionKeyParams {
                    agent_id: handle.id.clone(),
                    kind: MessageKind::DirectMessage { account_id: None },
                    channel: "cli".to_string(),
                    peer_id: peer_id.clone(),
                    dm_scope,
                });
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let msg = AgentMessage {
                    session_key,
                    text,
                    channel: "cli".to_string(),
                    peer_id,
                    chat_id: String::new(),
                    reply_tx,
                    task_id: None,
                    context_id: None,
                    event_tx: None,
                    cancel_token: None,
                    input_request_tx: None,
                    extra_tools: vec![],
                    images: vec![],
                    files: vec![],
                    account: None,
                };
                if handle.tx.send(msg).await.is_err() {
                    return;
                }
                if let Ok(Ok(reply)) =
                    tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await
                {
                    let pending = reply.pending_analysis;
                    if !reply.is_empty {
                        if let Err(e) = tx
                            .send(OutboundMessage {
                                target_id: "local".to_string(),
                                is_group: false,
                                text: reply.text,
                                reply_to: None,
                                images: reply.images,
                                channel: None,
                                files: reply.files,
                                account: None,
                            })
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
                            "local".to_string(),
                            false,
                            &cfg,
                        )
                        .await;
                    }
                }
            });
        });

        let cli_ch = Arc::new(CliChannel::new(on_message));
        let cli_send = Arc::clone(&cli_ch);

        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = cli_send.send(msg).await {
                    error!("CLI send error: {e:#}");
                }
            }
        });

        if let Err(e) = manager.register(Arc::clone(&cli_ch) as Arc<dyn Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        tokio::spawn(async move {
            if let Err(e) = cli_ch.run().await {
                error!("CLI channel error: {e:#}");
            }
        });
    }

    start_telegram_if_configured(
        config,
        registry.clone(),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );

    start_discord_if_configured(
        config,
        registry.clone(),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
    start_slack_if_configured(
        config,
        registry.clone(),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
    start_whatsapp_if_configured(
        config,
        registry.clone(),
        manager,
        whatsapp_slot,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
    start_line_if_configured(
        config,
        registry.clone(),
        manager,
        line_slot,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
    start_zalo_if_configured(
        config,
        registry.clone(),
        manager,
        zalo_slot,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
    start_signal_if_configured(
        config,
        registry.clone(),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
    start_wechat_personal_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
    start_feishu_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        Arc::clone(&feishu_slot),
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
    start_dingtalk_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
    start_qq_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
    start_matrix_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
    start_wecom_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        wecom_slot,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue),
        shutdown.clone(),
    );
}
