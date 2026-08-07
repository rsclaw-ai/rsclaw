use std::sync::Arc;

use rsclaw_agent::{AgentMessage, AgentRegistry};
use rsclaw_channel::{Channel, OutboundMessage};
use rsclaw_config::runtime::RuntimeConfig;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::{
    super::preparse::{btw_direct_call, is_fast_preparse, try_preparse_locally},
    default_dm_scope,
};
use crate::gateway::session::{MessageKind, SessionKeyParams, derive_session_key};

pub(crate) fn start_matrix_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &rsclaw_channel::ChannelManager,
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
    let Some(matrix_cfg) = &config.channel.channels.matrix else {
        return;
    };
    if !matrix_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = matrix_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(rsclaw_config::schema::DmPolicy::Pairing);
    let group_policy = matrix_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(rsclaw_config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> =
        matrix_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = matrix_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        rsclaw_channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("matrix", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("matrix".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, homeserver, access_token, user_id) tuples from
    // accounts.<name>.{homeserver?, accessToken, userId?}
    let mut mx_accounts: Vec<(String, String, String, String)> = Vec::new();
    if let Some(accts) = &matrix_cfg.accounts {
        for (name, acct) in accts {
            let token = acct
                .get("accessToken")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !token.is_empty() {
                let hs = acct
                    .get("homeserver")
                    .and_then(|v| v.as_str())
                    .unwrap_or("https://matrix.org")
                    .to_owned();
                let uid = acct
                    .get("userId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                mx_accounts.push((name.clone(), hs, token.to_owned(), uid));
            }
        }
    }

    if mx_accounts.is_empty() {
        warn!("matrix.accessToken not set in accounts, channel disabled");
        return;
    }

    for (acct_name, homeserver, access_token, user_id) in mx_accounts {
        let acct_for_log = acct_name.clone();
        let w_acct_outer = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg = config.clone();
        let tq = Arc::clone(&task_queue);
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register Matrix channel sender for notification routing.
        {
            let mut senders = channel_senders
                .write()
                .expect("channel_senders lock poisoned");
            senders.insert(format!("matrix/{}", acct_name), out_tx.clone());
            senders
                .entry("matrix".to_string())
                .or_insert_with(|| out_tx.clone());
        }

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());

        // Per-user inbound queue for Matrix.
        type MatrixItem = (
            String,
            String,
            String,
            bool,
            Vec<rsclaw_agent::registry::ImageAttachment>,
            Vec<rsclaw_agent::registry::FileAttachment>,
        );
        let matrix_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<MatrixItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender: String,
                  text: String,
                  room_id: String,
                  is_group: bool,
                  images: Vec<rsclaw_agent::registry::ImageAttachment>,
                  files: Vec<rsclaw_agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let tx = out_tx.clone();
                let cfg = cfg.clone();
                let tq = Arc::clone(&tq);
                let queues = Arc::clone(&matrix_user_queues);
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let w_acct_outer = w_acct_outer.clone();
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            rsclaw_config::schema::GroupPolicy::Disabled => {
                                warn!("matrix group message rejected: groupPolicy=disabled");
                                return;
                            }
                            rsclaw_config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == room_id) {
                                    warn!("matrix group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            rsclaw_config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_group {
                        use rsclaw_channel::PolicyResult;
                        match enforcer.check(&sender).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                warn!(peer_id = %sender, "matrix DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: room_id.clone(),
                                        is_group: false,
                                        text: rsclaw_i18n::t_fmt(
                                            "pairing_required",
                                            rsclaw_i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
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
                                        target_id: room_id.clone(),
                                        is_group: false,
                                        text: rsclaw_i18n::t(
                                            "pairing_queue_full",
                                            rsclaw_i18n::default_lang(),
                                        )
                                        .to_owned(),
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
                            let (utx, mut urx) = mpsc::channel::<MatrixItem>(32);
                            map.insert(sender.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = cfg.clone();
                            let w_uid = sender.clone();
                            let w_tq = Arc::clone(&tq);
                            let w_acct = w_acct_outer.clone();
                            tokio::spawn(async move {
                                while let Some((text, sender, room_id, is_group, images, files)) =
                                    urx.recv().await
                                {
                                    // No debounce — task queue merge_into_pending
                                    // handles rapid consecutive messages automatically.
                                    let handle = match w_reg
                                        .route("matrix")
                                        .or_else(|_| w_reg.default_agent())
                                    {
                                        Ok(h) => h,
                                        Err(e) => {
                                            error!("matrix route error: {e:#}");
                                            continue;
                                        }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: if is_group {
                                            MessageKind::GroupMessage {
                                                group_id: room_id.clone(),
                                                thread_id: None,
                                            }
                                        } else {
                                            MessageKind::DirectMessage {
                                                account_id: Some(w_acct.clone()),
                                            }
                                        },
                                        channel: "matrix".to_string(),
                                        peer_id: sender.clone(),
                                        dm_scope,
                                    });
                                    let qmsg = crate::gateway::task_queue::QueuedMessage {
                                        text,
                                        sender: sender.to_string(),
                                        channel: "matrix".to_string(),
                                        chat_id: room_id.clone(),
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
                                        error!(user = %w_uid, "matrix: queue submit failed: {e:#}");
                                    }
                                }
                                debug!(user = %w_uid, "matrix: per-user worker stopped");
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
                        let cfg = cfg.clone();
                        let w_acct_btw = w_acct_outer.clone();
                        let question = text[5..].to_owned();
                        let room_id = room_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg
                                .route_account("matrix", Some(&w_acct_btw))
                                .or_else(|_| reg.route_account("matrix", None))
                                .or_else(|_| reg.default_agent())
                            {
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
                                        target_id: room_id,
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
                        let cfg = cfg.clone();
                        let sender = sender.clone();
                        let room_id = room_id.clone();
                        let w_acct_pp = w_acct_outer.clone();
                        tokio::spawn(async move {
                            let handle = match reg
                                .route_account("matrix", Some(&w_acct_pp))
                                .or_else(|_| reg.route_account("matrix", None))
                                .or_else(|_| reg.default_agent())
                            {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: room_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage {
                                        account_id: Some(w_acct_pp.clone()),
                                    }
                                },
                                channel: "matrix".to_string(),
                                peer_id: sender.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(
                                &text,
                                &handle,
                                "matrix",
                                &sender,
                                crate::gateway::preparse::PreparseOrigin::User,
                            )
                            .await
                            {
                                reply.target_id = room_id.clone();
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
                                channel: "matrix".to_string(),
                                peer_id: sender,
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
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(10),
                                reply_rx,
                            )
                            .await
                            {
                                Ok(Ok(r)) => {
                                    if !r.is_empty {
                                        if let Err(e) = tx
                                            .send(OutboundMessage {
                                                target_id: room_id,
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
                                Ok(Err(_)) => {
                                    warn!("matrix: chat-mode agent reply error");
                                    let _ = tx
                                        .send(OutboundMessage {
                                            target_id: room_id.clone(),
                                            is_group,
                                            text: rsclaw_i18n::t(
                                                "chat_reply_error",
                                                rsclaw_i18n::default_lang(),
                                            ),
                                            reply_to: None,
                                            images: vec![],
                                            files: vec![],
                                            channel: None,
                                            account: Some(w_acct_pp.clone()),
                                        })
                                        .await;
                                }
                                Err(_) => {
                                    warn!("matrix: chat-mode agent reply timed out");
                                    let _ = tx
                                        .send(OutboundMessage {
                                            target_id: room_id.clone(),
                                            is_group,
                                            text: rsclaw_i18n::t(
                                                "chat_reply_timeout",
                                                rsclaw_i18n::default_lang(),
                                            ),
                                            reply_to: None,
                                            images: vec![],
                                            files: vec![],
                                            channel: None,
                                            account: Some(w_acct_pp.clone()),
                                        })
                                        .await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) =
                        user_tx.try_send((text, sender.clone(), room_id, is_group, images, files))
                    {
                        warn!(user = %sender, error = %e, "matrix: user queue full, dropping message");
                    }
                });
            },
        );

        let matrix = Arc::new({
            let ch = rsclaw_channel::matrix::MatrixChannel::new(
                homeserver,
                access_token,
                user_id,
                on_message,
            );
            #[cfg(feature = "channel-matrix")]
            {
                if let Some(did) = matrix_cfg.device_id.as_deref() {
                    ch = ch.with_device_id(did);
                }
                if let Some(rk) = matrix_cfg
                    .recovery_key
                    .as_ref()
                    .and_then(|s| s.resolve_early())
                {
                    ch = ch.with_recovery_key(rk);
                }
            }
            ch
        });

        let chan_name = format!("matrix/{}", acct_for_log);
        let cancel_token = manager.register_cancel_token(&chan_name);
        let cancel_for_out = cancel_token.clone();
        if let Err(e) = manager.register_with_name(
            chan_name,
            Arc::clone(&matrix) as Arc<dyn rsclaw_channel::Channel>,
        ) {
            tracing::warn!("failed to register channel: {e}");
        }
        let matrix_send = Arc::clone(&matrix);
        let shutdown_for_out = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown_for_out.notified() => {
                        info!("matrix: drain signaled, stopping outbound sender");
                        break;
                    }
                    () = cancel_for_out.cancelled() => {
                        info!("matrix: channel cancelled, stopping outbound sender");
                        break;
                    }
                    msg = out_rx.recv() => {
                        let Some(msg) = msg else { break };
                        if let Err(e) = matrix_send.send(msg).await {
                            error!("matrix send error: {e:#}");
                        }
                    }
                }
            }
        });

        let shutdown_for_run = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                res = matrix.run() => {
                    if let Err(e) = res {
                        error!("matrix channel error: {e:#}");
                    }
                }
                () = shutdown_for_run.notified() => {
                    info!("matrix: drain signaled, stopping run loop");
                }
                () = cancel_token.cancelled() => {
                    info!("matrix: channel cancelled, stopping run loop");
                }
            }
        });

        info!(account = %acct_for_log, "matrix channel started");
    } // end for mx_accounts
}
