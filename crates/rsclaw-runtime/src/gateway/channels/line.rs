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

pub(crate) fn start_line_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut rsclaw_channel::ChannelManager,
    line_slot: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::line::LineChannel>>>,
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
    use rsclaw_channel::line::LineChannel;

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
        .unwrap_or(rsclaw_config::schema::DmPolicy::Pairing);
    let group_policy = line_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(rsclaw_config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = line_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = line_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        rsclaw_channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("line", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("line".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, access_token) pairs from
    // accounts.<name>.channelAccessToken.
    let mut line_accounts: Vec<(String, String)> = Vec::new();
    if let Some(accts) = &line_cfg.accounts {
        for (name, acct) in accts {
            let t = acct
                .get("channelAccessToken")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !t.is_empty() {
                line_accounts.push((name.clone(), t.to_owned()));
            }
        }
    }

    if line_accounts.is_empty() {
        warn!("line.channelAccessToken not set in accounts, channel disabled");
        return;
    }

    for (acct_name, channel_access_token) in line_accounts {
        let acct_for_log = acct_name.clone();
        let w_acct_outer = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register LINE channel sender for notification routing.
        {
            let mut senders = channel_senders
                .write()
                .expect("channel_senders lock poisoned");
            senders.insert(format!("line/{}", acct_name), out_tx.clone());
            senders
                .entry("line".to_string())
                .or_insert_with(|| out_tx.clone());
        }

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());
        let tq = Arc::clone(&task_queue);

        // Per-user inbound queue for LINE.
        type LineItem = (
            String,
            String,
            bool,
            Vec<rsclaw_agent::registry::ImageAttachment>,
        );
        let line_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<LineItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |user_id: String,
                  text: String,
                  is_group: bool,
                  images: Vec<rsclaw_agent::registry::ImageAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&line_user_queues);
                let tq = Arc::clone(&tq);
                let w_acct_outer = w_acct_outer.clone();
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            rsclaw_config::schema::GroupPolicy::Disabled => {
                                warn!("line group message rejected: groupPolicy=disabled");
                                return;
                            }
                            rsclaw_config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == user_id) {
                                    warn!("line group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            rsclaw_config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_group {
                        use rsclaw_channel::PolicyResult;
                        match enforcer.check(&user_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                warn!(peer_id = %user_id, "line DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: user_id.clone(),
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
                                        target_id: user_id.clone(),
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
                            let w_uid = user_id.clone();
                            let w_tq = Arc::clone(&tq);
                            let w_acct = w_acct_outer.clone();
                            tokio::spawn(async move {
                                while let Some((text, user_id, is_group, images)) = urx.recv().await
                                {
                                    // No debounce — task queue merge_into_pending
                                    // handles rapid consecutive messages automatically.
                                    let handle = match w_reg
                                        .route_account("line", Some(&w_acct))
                                        .or_else(|_| w_reg.route_account("line", None))
                                    {
                                        Ok(h) => h,
                                        Err(e) => {
                                            error!("line route: {e:#}");
                                            continue;
                                        }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: if is_group {
                                            MessageKind::GroupMessage {
                                                group_id: user_id.clone(),
                                                thread_id: None,
                                            }
                                        } else {
                                            MessageKind::DirectMessage {
                                                account_id: Some(w_acct.clone()),
                                            }
                                        },
                                        channel: "line".to_string(),
                                        peer_id: user_id.clone(),
                                        dm_scope,
                                    });
                                    let qmsg = crate::gateway::task_queue::QueuedMessage {
                                        text,
                                        sender: user_id.clone(),
                                        channel: "line".to_string(),
                                        chat_id: String::new(),
                                        is_group,
                                        reply_to: None,
                                        timestamp: chrono::Utc::now().timestamp(),
                                        images: images.iter().map(|i| i.data.clone()).collect(),
                                        files: vec![],
                                        account: Some(w_acct.clone()),
                                    };
                                    if let Err(e) = w_tq.submit(
                                        &session_key,
                                        qmsg,
                                        crate::gateway::task_queue::Priority::User,
                                    ) {
                                        error!(user = %w_uid, "line: queue submit failed: {e:#}");
                                    }
                                }
                                debug!(user = %w_uid, "line: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&user_id).expect("queue entry must exist").clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let user_id = user_id.clone();
                        let w_acct_btw = w_acct_outer.clone();
                        tokio::spawn(async move {
                            let handle = match reg
                                .route_account("line", Some(&w_acct_btw))
                                .or_else(|_| reg.route_account("line", None))
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
                                        target_id: user_id,
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
                        let cfg = Arc::clone(&cfg);
                        let user_id = user_id.clone();
                        let w_acct_pp = w_acct_outer.clone();
                        tokio::spawn(async move {
                            let handle = match reg
                                .route_account("line", Some(&w_acct_pp))
                                .or_else(|_| reg.route_account("line", None))
                            {
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
                                    MessageKind::DirectMessage {
                                        account_id: Some(w_acct_pp.clone()),
                                    }
                                },
                                channel: "line".to_string(),
                                peer_id: user_id.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(
                                &text,
                                &handle,
                                "line",
                                &user_id,
                                crate::gateway::preparse::PreparseOrigin::User,
                            )
                            .await
                            {
                                reply.target_id = user_id.clone();
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
                                channel: "line".to_string(),
                                peer_id: user_id.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                task_id: None,
                                context_id: None,
                                event_tx: None,
                                cancel_token: None,
                                input_request_tx: None,
                                extra_tools: vec![],
                                images,
                                files: vec![],
                                account: Some(w_acct_pp.clone()),
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(Ok(r)) =
                                tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx)
                                    .await
                            {
                                if !r.is_empty {
                                    if let Err(e) = tx
                                        .send(OutboundMessage {
                                            target_id: user_id,
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
        if line_slot.set(Arc::clone(&line)).is_err() {
            tracing::debug!("slot already set, skipping");
        }
        let line_send = Arc::clone(&line);
        let shutdown_for_out = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown_for_out.notified() => {
                        info!("line: drain signaled, stopping outbound sender");
                        break;
                    }
                    msg = out_rx.recv() => {
                        let Some(msg) = msg else { break };
                        if let Err(e) = line_send.send(msg).await {
                            error!("line send: {e:#}");
                        }
                    }
                }
            }
        });
        if let Err(e) = manager.register(Arc::clone(&line) as Arc<dyn Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        let shutdown_for_run = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                res = line.run() => {
                    if let Err(e) = res {
                        error!("line channel: {e:#}");
                    }
                }
                () = shutdown_for_run.notified() => {
                    info!("line: drain signaled, stopping run loop");
                }
            }
        });
        info!(account = %acct_for_log, "line channel started (webhook mode)");
    } // end for line_accounts
}
