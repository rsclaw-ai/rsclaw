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

pub(crate) fn start_matrix_if_configured(
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
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = matrix_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> =
        matrix_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = matrix_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("matrix", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("matrix".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, homeserver, access_token, user_id) tuples.
    let mut mx_accounts: Vec<(String, String, String, String)> = Vec::new();

    // Legacy: single credentials at top level.
    if let Some(token) = matrix_cfg
        .access_token
        .as_ref()
        .and_then(|s| s.resolve_early())
        .filter(|s| !s.is_empty())
    {
        let hs = matrix_cfg
            .homeserver
            .as_deref()
            .unwrap_or("https://matrix.org")
            .to_owned();
        let uid = matrix_cfg.user_id.as_deref().unwrap_or("").to_owned();
        mx_accounts.push(("default".to_owned(), hs, token, uid));
    }

    // Multi-account: channels.matrix.accounts.<name>.{homeserver?, accessToken,
    // userId?}
    if let Some(accts) = &matrix_cfg.accounts {
        for (name, acct) in accts {
            let token = acct
                .get("accessToken")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !token.is_empty() && !mx_accounts.iter().any(|(_, _, et, _)| et == token) {
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
        warn!("matrix accessToken not set, channel disabled");
        return;
    }

    for (acct_name, homeserver, access_token, user_id) in mx_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg = config.clone();
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());

        // Per-user inbound queue for Matrix.
        type MatrixItem = (
            String,
            String,
            String,
            bool,
            Vec<crate::agent::registry::ImageAttachment>,
            Vec<crate::agent::registry::FileAttachment>,
        );
        let matrix_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<MatrixItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender: String,
                  text: String,
                  room_id: String,
                  is_group: bool,
                  images: Vec<crate::agent::registry::ImageAttachment>,
                  files: Vec<crate::agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let tx = out_tx.clone();
                let cfg = cfg.clone();
                let queues = Arc::clone(&matrix_user_queues);
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("matrix group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == room_id) {
                                    debug!("matrix group message rejected: not in groupAllowFrom");
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
                                debug!(peer_id = %sender, "matrix DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: room_id.clone(),
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
                                        target_id: room_id.clone(),
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
                            let w_tx = tx.clone();
                            let w_uid = sender.clone();
                            tokio::spawn(async move {
                                while let Some((mut text, sender, room_id, is_group, mut images, mut files)) =
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
                            let handle = match w_reg.route("matrix").or_else(|_| w_reg.default_agent()) {
                                Ok(h) => h,
                                Err(e) => { error!("matrix route error: {e:#}"); return; }
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
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "matrix".to_string(),
                                peer_id: sender.clone(),
                                dm_scope,
                            });
                            let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                            let msg = crate::agent::AgentMessage {
                                session_key,
                                text,
                                channel: "matrix".to_owned(),
                                peer_id: sender.clone(),
                                chat_id: room_id.clone(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files,
                                is_internal: false,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            let reply = tokio::select! {
                                result = &mut reply_rx => result,
                                _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                    send_processing(&w_tx, room_id.clone(), is_group, &w_cfg).await;
                                    reply_rx.await
                                }
                            };
                            match reply {
                                Ok(r) => {
                                    let pending = r.pending_analysis;
                                    if !r.is_empty {
                                        if let Err(e) = w_tx.send(OutboundMessage {
                                            target_id: room_id.clone(),
                                            is_group,
                                            text: r.text,
                                            reply_to: None,
                                            images: r.images,
                                            files: r.files,
                                            channel: None,                                        }).await
                                        {
                                            tracing::warn!("failed to send message: {e}");
                                        }
                                    }
                                    if let Some(analysis) = pending {
                                        handle_pending_analysis(
                                            analysis, Arc::clone(&handle), &w_tx,
                                            room_id, is_group, &w_cfg,
                                        ).await;
                                    }
                                }
                                _ => {}
                            }
                                }
                            ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "matrix: message processing timed out (600s), skipping to next");
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
                        let question = text[5..].to_owned();
                        let room_id = room_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("matrix").or_else(|_| reg.default_agent())
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
                        let cfg = cfg.clone();
                        let sender = sender.clone();
                        let room_id = room_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("matrix").or_else(|_| reg.default_agent())
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
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "matrix".to_string(),
                                peer_id: sender.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(&text, &handle).await {
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
                                extra_tools: vec![],
                                images,
                                files,
                                is_internal: false,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await {
                                if !r.is_empty {
                                    if let Err(e) = tx.send(OutboundMessage {
                                        target_id: room_id,
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
                        user_tx.try_send((text, sender.clone(), room_id, is_group, images, files))
                    {
                        warn!(user = %sender, error = %e, "matrix: user queue full, dropping message");
                    }
                });
            },
        );

        let matrix = Arc::new({
            let ch = crate::channel::matrix::MatrixChannel::new(
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

        if let Err(e) = manager.register(Arc::clone(&matrix) as Arc<dyn crate::channel::Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        let matrix_send = Arc::clone(&matrix);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = matrix_send.send(msg).await {
                    error!("matrix send error: {e:#}");
                }
            }
        });

        tokio::spawn(async move {
            if let Err(e) = matrix.run().await {
                error!("matrix channel error: {e:#}");
            }
        });

        info!(account = %acct_for_log, "matrix channel started");
    } // end for mx_accounts
}
