use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::{
    agent::{AgentMessage, AgentRegistry},
    channel::{Channel, OutboundMessage},
    config::runtime::RuntimeConfig,
    gateway::session::{MessageKind, SessionKeyParams, derive_session_key},
};

use super::super::preparse::{
    btw_direct_call, is_fast_preparse, try_preparse_locally,
};
use super::default_dm_scope;

pub(crate) fn start_zalo_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    zalo_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::zalo::ZaloChannel>>>,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
    task_queue: Arc<crate::gateway::task_queue::TaskQueueManager>,
) {
    use crate::channel::zalo::ZaloChannel;

    let Some(zalo_cfg) = &config.channel.channels.zalo else {
        return;
    };
    if !zalo_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy from config (Zalo is DM-only, no group policy needed).
    let dm_policy = zalo_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = zalo_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("zalo", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("zalo".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, access_token) pairs.
    let mut zalo_accounts: Vec<(String, String)> = Vec::new();

    // Legacy: single token at top level.
    if let Some(token) = zalo_cfg
        .access_token
        .as_ref()
        .and_then(|t| t.as_plain())
        .map(str::to_owned)
        .or_else(|| std::env::var("ZALO_ACCESS_TOKEN").ok())
    {
        zalo_accounts.push(("default".to_owned(), token));
    }

    // Multi-account: channels.zalo.accounts.<name>.accessToken
    if let Some(accts) = &zalo_cfg.accounts {
        for (name, acct) in accts {
            if let Some(t) = acct.get("accessToken").and_then(|v| v.as_str()) {
                if !zalo_accounts.iter().any(|(_, et)| et == t) {
                    zalo_accounts.push((name.clone(), t.to_owned()));
                }
            }
        }
    }

    if zalo_accounts.is_empty() {
        warn!("ZALO access_token not set, zalo disabled");
        return;
    }

    for (acct_name, access_token) in zalo_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);
        let tq = Arc::clone(&task_queue);

        // Per-user inbound queue for Zalo.
        type ZaloItem = (String, String, Vec<crate::agent::registry::ImageAttachment>);
        let zalo_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<ZaloItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender_id: String,
                  text: String,
                  images: Vec<crate::agent::registry::ImageAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let enforcer = Arc::clone(&enforcer);
                let queues = Arc::clone(&zalo_user_queues);
                let tq = Arc::clone(&tq);
                tokio::spawn(async move {
                    // DM policy check (Zalo is DM-only).
                    {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&sender_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %sender_id, "zalo DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: sender_id.clone(),
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
                                        target_id: sender_id.clone(),
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
                        let needs_create = match map.get(&sender_id) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&sender_id);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<ZaloItem>(32);
                            map.insert(sender_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_uid = sender_id.clone();
                            let w_tq = Arc::clone(&tq);
                            tokio::spawn(async move {
                                while let Some((text, sender_id, images)) = urx.recv().await {
                                    // No debounce — task queue merge_into_pending
                                    // handles rapid consecutive messages automatically.
                                    let handle = match w_reg.route("zalo") {
                                        Ok(h) => h,
                                        Err(e) => { error!("zalo route: {e:#}"); continue; }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: MessageKind::DirectMessage { account_id: None },
                                        channel: "zalo".to_string(),
                                        peer_id: sender_id.clone(),
                                        dm_scope,
                                    });
                                    let qmsg = crate::gateway::task_queue::QueuedMessage {
                                        text,
                                        sender: sender_id.clone(),
                                        channel: "zalo".to_string(),
                                        chat_id: String::new(),
                                        is_group: false,
                                        timestamp: chrono::Utc::now().timestamp(),
                                        images: images.iter().map(|i| i.data.clone()).collect(),
                                        files: vec![],
                                    };
                                    if let Err(e) = w_tq.submit(&session_key, qmsg, crate::gateway::task_queue::Priority::User) {
                                        error!(user = %w_uid, "zalo: queue submit failed: {e:#}");
                                    }
                                }
                                debug!(user = %w_uid, "zalo: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&sender_id).expect("queue entry must exist").clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let sender_id = sender_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("zalo") {
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
                                        target_id: sender_id,
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
                        let sender_id = sender_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("zalo") {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: MessageKind::DirectMessage { account_id: None },
                                channel: "zalo".to_string(),
                                peer_id: sender_id.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(&text, &handle).await {
                                reply.target_id = sender_id.clone();
                                reply.is_group = false;
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
                                channel: "zalo".to_string(),
                                peer_id: sender_id.clone(),
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
                                    if let Err(e) = tx.send(OutboundMessage {
                                        target_id: sender_id,
                                        is_group: false,
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
                    if let Err(e) = user_tx.try_send((text, sender_id.clone(), images)) {
                        warn!(user = %sender_id, error = %e, "zalo: user queue full, dropping message");
                    }
                });
            },
        );

        let zalo = Arc::new(ZaloChannel::with_api_base(
            access_token,
            zalo_cfg.api_base.clone(),
            on_message,
        ));
        if zalo_slot.set(Arc::clone(&zalo)).is_err() {
            tracing::debug!("slot already set, skipping");
        }
        let zalo_send = Arc::clone(&zalo);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = zalo_send.send(msg).await {
                    error!("zalo send: {e:#}");
                }
            }
        });
        if let Err(e) = manager.register(Arc::clone(&zalo) as Arc<dyn Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        tokio::spawn(async move {
            if let Err(e) = zalo.run().await {
                error!("zalo channel: {e:#}");
            }
        });
        info!(account = %acct_for_log, "zalo channel started (webhook mode)");
    } // end for zalo_accounts
}
