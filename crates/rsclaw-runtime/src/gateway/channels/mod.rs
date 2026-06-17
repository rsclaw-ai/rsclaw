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
mod wechat;
mod wecom;
mod whatsapp;
mod zalo;

use std::sync::Arc;

pub(crate) use custom::start_custom_channels;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use self::{
    dingtalk::start_dingtalk_if_configured, discord::start_discord_if_configured,
    feishu::start_feishu_if_configured, line::start_line_if_configured,
    matrix::start_matrix_if_configured, qq::start_qq_if_configured,
    signal::start_signal_if_configured, slack::start_slack_if_configured,
    wechat::start_wechat_personal_if_configured, wecom::start_wecom_if_configured,
    whatsapp::start_whatsapp_if_configured, zalo::start_zalo_if_configured,
};
use super::{
    preparse::{btw_direct_call, is_fast_preparse, try_preparse_locally},
    startup::handle_pending_analysis,
};
use crate::gateway::session::{MessageKind, SessionKeyParams, derive_session_key};
use rsclaw_agent::{AgentMessage, AgentRegistry};
use rsclaw_channel::{Channel, OutboundMessage, cli::CliChannel, telegram::TelegramChannel};
use rsclaw_config::{runtime::RuntimeConfig, schema::DmScope};

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
    manager: &mut rsclaw_channel::ChannelManager,
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

    // Telegram — supports multiple accounts (OpenClaw format).
    if let Some(tg_cfg) = &config.channel.channels.telegram
        && tg_cfg.base.enabled.unwrap_or(true)
    {
        // Collect (account_name, bot_token) pairs from
        // accounts.<name>.botToken.
        let mut tg_accounts: Vec<(String, String)> = Vec::new();
        if let Some(accts) = &tg_cfg.accounts {
            for (name, acct) in accts {
                let t = acct.get("botToken").and_then(|v| v.as_str()).unwrap_or("");
                if !t.is_empty() {
                    tg_accounts.push((name.clone(), t.to_owned()));
                }
            }
        }

        if tg_accounts.is_empty() {
            warn!("telegram.botToken not set in accounts, channel disabled");
        }

        // Load dmPolicy and groupPolicy from config.
        let dm_policy = tg_cfg
            .base
            .dm_policy
            .clone()
            .unwrap_or(rsclaw_config::schema::DmPolicy::Pairing);
        let group_policy = tg_cfg
            .base
            .group_policy
            .clone()
            .unwrap_or(rsclaw_config::schema::GroupPolicy::Allowlist);
        let group_allow_from: Vec<String> =
            tg_cfg.base.group_allow_from.clone().unwrap_or_default();
        let allow_from: Vec<String> = tg_cfg.base.allow_from.clone().unwrap_or_default();

        let enforcer = Arc::new(
            rsclaw_channel::DmPolicyEnforcer::new(dm_policy.clone(), allow_from)
                .with_persistence("telegram", Arc::clone(&redb_store)),
        );

        // Register enforcer so the pairing API can approve codes.
        if let Ok(mut enforcers) = dm_enforcers.write() {
            enforcers.insert("telegram".to_owned(), Arc::clone(&enforcer));
        }

        for (acct_name, token) in tg_accounts {
            // Find binding for this account to determine which agent handles it.
            let bound_agent = config
                .agents
                .bindings
                .iter()
                .find(|b| {
                    b.match_.channel.as_deref() == Some("telegram")
                        && b.match_.account_id.as_deref() == Some(&acct_name)
                })
                .map(|b| b.agent_id.clone());

            let reg = Arc::clone(&registry);
            let cfg_arc = Arc::new(config.clone());
            let acct_for_log = acct_name.clone();
            let w_acct_outer = acct_name.clone();
            let bound = bound_agent.clone();
            let enforcer = Arc::clone(&enforcer);
            let gp = Arc::new(group_policy.clone());
            let ga = Arc::new(group_allow_from.clone());
            let tq = Arc::clone(&task_queue);
            let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

            // Register Telegram channel sender for notification routing.
            // Both bare "telegram" (for task queue routing) and "telegram/{account}"
            // (multi-account).
            {
                let mut senders = channel_senders
                    .write()
                    .expect("channel_senders lock poisoned");
                senders.insert(format!("telegram/{}", acct_name), out_tx.clone());
                senders
                    .entry("telegram".to_string())
                    .or_insert_with(|| out_tx.clone());
            }

            // Per-user inbound queue: serializes messages so each user's messages
            // are processed one at a time, preventing reply channel drops.
            type TgItem = (
                String,
                i64,
                i64,
                bool,
                Option<String>,
                Vec<rsclaw_agent::registry::ImageAttachment>,
                Vec<rsclaw_agent::registry::FileAttachment>,
            );
            let tg_user_queues: Arc<
                tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<TgItem>>>,
            > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

            let on_message = Arc::new(
                move |peer_id: i64,
                      text: String,
                      chat_id: i64,
                      is_group: bool,
                      _thread: Option<i64>,
                      images: Vec<rsclaw_agent::registry::ImageAttachment>,
                      file_attachments: Vec<rsclaw_agent::registry::FileAttachment>| {
                    let reg = Arc::clone(&reg);
                    let cfg = Arc::clone(&cfg_arc);
                    let tx = out_tx.clone();
                    let bound = bound.clone();
                    let enforcer = Arc::clone(&enforcer);
                    let group_policy = Arc::clone(&gp);
                    let group_allow = Arc::clone(&ga);
                    let queues = Arc::clone(&tg_user_queues);
                    let tq = Arc::clone(&tq);
                    let w_acct_outer = w_acct_outer.clone();
                    tokio::spawn(async move {
                        // Group policy check.
                        if is_group {
                            match group_policy.as_ref() {
                                rsclaw_config::schema::GroupPolicy::Disabled => {
                                    warn!(chat_id, "telegram group message rejected: groupPolicy=disabled");
                                    return;
                                }
                                rsclaw_config::schema::GroupPolicy::Allowlist => {
                                    let cid = chat_id.to_string();
                                    if !group_allow.iter().any(|g| *g == cid) {
                                        warn!(chat_id, "telegram group message rejected: not in groupAllowFrom");
                                        return;
                                    }
                                }
                                rsclaw_config::schema::GroupPolicy::Open => {}
                            }
                        }
                        // DM policy check.
                        if !is_group {
                            use rsclaw_channel::PolicyResult;
                            match enforcer.check(&peer_id.to_string()).await {
                                PolicyResult::Allow => {}
                                PolicyResult::Deny => {
                                    warn!(peer_id, "telegram DM rejected by policy");
                                    return;
                                }
                                PolicyResult::SendPairingCode(code) => {
                                    if let Err(e) = tx
                                        .send(OutboundMessage {
                                            target_id: chat_id.to_string(),
                                            is_group: false,
                                            text: rsclaw_i18n::t_fmt("pairing_required", rsclaw_i18n::default_lang(), &[("code", &code)]),
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
                                            target_id: chat_id.to_string(),
                                            is_group: false,
                                            text: rsclaw_i18n::t("pairing_queue_full", rsclaw_i18n::default_lang()).to_owned(),
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
                        let queue_key = peer_id.to_string();
                        let user_tx = {
                            let mut map = queues.lock().await;
                            let needs_create = match map.get(&queue_key) {
                                Some(existing) if !existing.is_closed() => false,
                                Some(_) => { map.remove(&queue_key); true }
                                None => true,
                            };
                            if needs_create {
                                let (utx, mut urx) = mpsc::channel::<TgItem>(32);
                                map.insert(queue_key.clone(), utx.clone());
                                let w_reg = Arc::clone(&reg);
                                let w_cfg = Arc::clone(&cfg);
                                let w_uid = queue_key.clone();
                                let w_tq = Arc::clone(&tq);
                                let w_acct = w_acct_outer.clone();
                                tokio::spawn(async move {
                                    while let Some((text, peer_id, chat_id, is_group, bound, images, file_attachments)) = urx.recv().await {
                                        // No debounce — task queue merge_into_pending
                                        // handles rapid consecutive messages automatically.
                                        let handle = if let Some(ref agent_id) = bound {
                                            match w_reg.get(agent_id) {
                                                Ok(h) => h,
                                                Err(_) => match w_reg.route_account("telegram", Some(&w_acct)) {
                                                    Ok(h) => h,
                                                    Err(_) => match w_reg.route_account("telegram", None) {
                                                        Ok(h) => h,
                                                        Err(e) => { error!("route error: {e:#}"); continue; }
                                                    },
                                                },
                                            }
                                        } else {
                                            match w_reg.route_account("telegram", Some(&w_acct)) {
                                                Ok(h) => h,
                                                Err(_) => match w_reg.route_account("telegram", None) {
                                                    Ok(h) => h,
                                                    Err(e) => { error!("route error: {e:#}"); continue; }
                                                },
                                            }
                                        };
                                        let dm_scope = default_dm_scope(&w_cfg);
                                        let session_key = derive_session_key(&SessionKeyParams {
                                            agent_id: handle.id.clone(),
                                            kind: if is_group {
                                                MessageKind::GroupMessage {
                                                    group_id: chat_id.to_string(),
                                                    thread_id: None,
                                                }
                                            } else {
                                                MessageKind::DirectMessage { account_id: Some(w_acct.clone()) }
                                            },
                                            channel: "telegram".to_string(),
                                            peer_id: peer_id.to_string(),
                                            dm_scope,
                                        });
                                        let qmsg = crate::gateway::task_queue::QueuedMessage {
                                            text,
                                            sender: peer_id.to_string(),
                                            channel: "telegram".to_string(),
                                            chat_id: chat_id.to_string(),
                                            is_group,
                                            reply_to: None,
                                            timestamp: chrono::Utc::now().timestamp(),
                                            images: images.iter().map(|i| i.data.clone()).collect(),
                                            files: file_attachments.iter().filter_map(|f| {
                                                crate::gateway::task_queue::stage_file(&f.filename, &f.data, &f.mime_type).ok()
                                            }).collect(),
                                            account: Some(w_acct.clone()),
                                        };
                                        if let Err(e) = w_tq.submit(&session_key, qmsg, crate::gateway::task_queue::Priority::User) {
                                            error!(user = %w_uid, "telegram: queue submit failed: {e:#}");
                                        }
                                    }
                                    debug!(user = %w_uid, "telegram: per-user worker stopped");
                                });
                                utx
                            } else {
                                map.get(&queue_key).expect("queue entry must exist").clone()
                            }
                        };
                        // /btw bypass: spawn directly, skip the per-user queue
                        if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                            let reg = Arc::clone(&reg);
                            let tx = tx.clone();
                            let cfg = Arc::clone(&cfg);
                            let question = text[5..].to_owned();
                            let chat_id_s = chat_id.to_string();
                            let bound = bound.clone();
                            let w_acct_btw = w_acct_outer.clone();
                            tokio::spawn(async move {
                                let handle = if let Some(ref agent_id) = bound {
                                    match reg.get(agent_id) {
                                        Ok(h) => h,
                                        Err(_) => match reg.route_account("telegram", Some(&w_acct_btw)) {
                                            Ok(h) => h,
                                            Err(_) => match reg.route_account("telegram", None) {
                                                Ok(h) => h,
                                                Err(_) => return,
                                            },
                                        },
                                    }
                                } else {
                                    match reg.route_account("telegram", Some(&w_acct_btw)) {
                                        Ok(h) => h,
                                        Err(_) => match reg.route_account("telegram", None) {
                                            Ok(h) => h,
                                            Err(_) => return,
                                        },
                                    }
                                };
                                if let Some(reply_text) = btw_direct_call(
                                    &question, &handle.live_status, &handle.providers, &cfg,
                                ).await {
                                    if let Err(e) = tx.send(OutboundMessage {
                                        target_id: chat_id_s,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,
                                        account: Some(w_acct_btw.clone()),
                                        files: vec![],
                                    }).await
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
                            let peer_id_s = peer_id.to_string();
                            let chat_id_s = chat_id.to_string();
                            let bound = bound.clone();
                            let w_acct_pp = w_acct_outer.clone();
                            tokio::spawn(async move {
                                let handle = if let Some(ref agent_id) = bound {
                                    match reg.get(agent_id) {
                                        Ok(h) => h,
                                        Err(_) => match reg.route_account("telegram", Some(&w_acct_pp)) {
                                            Ok(h) => h,
                                            Err(_) => match reg.route_account("telegram", None) {
                                                Ok(h) => h,
                                                Err(_) => return,
                                            },
                                        },
                                    }
                                } else {
                                    match reg.route_account("telegram", Some(&w_acct_pp)) {
                                        Ok(h) => h,
                                        Err(_) => match reg.route_account("telegram", None) {
                                            Ok(h) => h,
                                            Err(_) => return,
                                        },
                                    }
                                };
                                let dm_scope = default_dm_scope(&cfg);
                                let session_key = derive_session_key(&SessionKeyParams {
                                    agent_id: handle.id.clone(),
                                    kind: if is_group {
                                        MessageKind::GroupMessage {
                                            group_id: chat_id_s.clone(),
                                            thread_id: None,
                                        }
                                    } else {
                                        MessageKind::DirectMessage { account_id: Some(w_acct_pp.clone()) }
                                    },
                                    channel: "telegram".to_string(),
                                    peer_id: peer_id_s.clone(),
                                    dm_scope,
                                });
                                if let Some(mut reply) = try_preparse_locally(&text, &handle, "telegram", &peer_id_s, crate::gateway::preparse::PreparseOrigin::User).await {
                                    reply.target_id = chat_id_s;
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
                                    channel: "telegram".to_string(),
                                    peer_id: peer_id_s,
                                    chat_id: String::new(),
                                    reply_tx,
                                    task_id: None,
                                    context_id: None,
                                    event_tx: None,
                                    cancel_token: None,
                                    input_request_tx: None,
                                    extra_tools: vec![],
                                    images,
                                    files: file_attachments,
                                    account: Some(w_acct_pp.clone()),
                                };
                                if handle.tx.send(msg).await.is_err() {
                                    return;
                                }
                                if let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await {
                                    if !r.is_empty {
                                        if let Err(e) = tx.send(OutboundMessage {
                                            target_id: chat_id_s,
                                            is_group,
                                            text: r.text,
                                            reply_to: None,
                                            images: r.images,
                                            files: r.files,
                                            channel: None,
                                            account: Some(w_acct_pp.clone()),
                                        }).await
                                        {
                                            tracing::warn!("failed to send message: {e}");
                                        }
                                    }
                                }
                            });
                            return;
                        }
                        if let Err(e) = user_tx.try_send((text, peer_id, chat_id, is_group, bound, images, file_attachments)) {
                            warn!(user = %queue_key, error = %e, "telegram: user queue full, dropping message");
                        }
                    });
                },
            );

            let api_base = tg_cfg.api_base.clone();
            let tg = Arc::new(TelegramChannel::new(token, api_base, on_message));
            let tg_send = Arc::clone(&tg);
            let shutdown_for_out = shutdown.clone();

            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = shutdown_for_out.notified() => {
                            info!("telegram: drain signaled, stopping outbound sender");
                            break;
                        }
                        msg = out_rx.recv() => {
                            let Some(msg) = msg else { break };
                            if let Err(e) = tg_send.send(msg).await {
                                error!("telegram send error: {e:#}");
                            }
                        }
                    }
                }
            });

            if let Err(e) = manager.register_with_name(
                format!("telegram/{}", acct_for_log),
                Arc::clone(&tg) as Arc<dyn Channel>,
            ) {
                tracing::warn!("failed to register channel: {e}");
            }
            let shutdown_for_run = shutdown.clone();
            tokio::spawn(async move {
                tokio::select! {
                    res = tg.run() => {
                        if let Err(e) = res {
                            error!("telegram channel error: {e:#}");
                        }
                    }
                    () = shutdown_for_run.notified() => {
                        info!("telegram: drain signaled, stopping run loop");
                    }
                }
            });
            info!(account = %acct_for_log, "telegram channel started");
        }
    }

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
