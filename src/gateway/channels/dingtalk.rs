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

// ---------------------------------------------------------------------------
// DingTalk (钉钉)
// ---------------------------------------------------------------------------

pub(crate) fn start_dingtalk_if_configured(
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
    task_queue: Arc<crate::gateway::task_queue::TaskQueueManager>,
) {
    let Some(dt_cfg) = &config.channel.channels.dingtalk else {
        return;
    };
    if !dt_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Collect (account_name, app_key, app_secret, robot_code) tuples.
    let mut dt_accounts: Vec<(String, String, String, String)> = Vec::new();

    // Legacy: single appKey/appSecret at top level.
    if let (Some(key), Some(secret)) = (
        dt_cfg
            .app_key
            .as_deref()
            .filter(|s| !s.starts_with("YOUR_")),
        dt_cfg
            .app_secret
            .as_ref()
            .and_then(|s| s.as_plain())
            .filter(|s| !s.starts_with("YOUR_")),
    ) {
        let robot = dt_cfg.robot_code.clone().unwrap_or_else(|| key.to_owned());
        dt_accounts.push((
            "default".to_owned(),
            key.to_owned(),
            secret.to_owned(),
            robot,
        ));
    }

    // Multi-account: channels.dingtalk.accounts.<name>.{appKey, appSecret,
    // robotCode?}
    if let Some(accts) = &dt_cfg.accounts {
        for (name, acct) in accts {
            let key = acct.get("appKey").and_then(|v| v.as_str()).unwrap_or("");
            let secret = acct.get("appSecret").and_then(|v| v.as_str()).unwrap_or("");
            if !key.is_empty() && !secret.is_empty() {
                if !dt_accounts.iter().any(|(_, ek, _, _)| ek == key) {
                    let robot = acct
                        .get("robotCode")
                        .and_then(|v| v.as_str())
                        .unwrap_or(key)
                        .to_owned();
                    dt_accounts.push((name.clone(), key.to_owned(), secret.to_owned(), robot));
                }
            }
        }
    }

    if dt_accounts.is_empty() {
        warn!("dingtalk appKey not set, channel disabled");
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = dt_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = dt_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = dt_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = dt_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("dingtalk", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("dingtalk".to_owned(), Arc::clone(&enforcer));
    }

    for (acct_name, app_key, app_secret, robot_code) in dt_accounts {
        let reg = Arc::clone(&registry);
        let cfg = config.clone();
        let tq = Arc::clone(&task_queue);
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Find binding for this account to determine which agent handles it.
        let bound_agent = config
            .agents
            .bindings
            .iter()
            .find(|b| {
                b.match_.channel.as_deref() == Some("dingtalk")
                    && b.match_.account_id.as_deref() == Some(&acct_name)
            })
            .map(|b| b.agent_id.clone());
        let bound = bound_agent.clone();

        // Per-user inbound queue for DingTalk.
        type DtItem = (
            String,
            String,
            String,
            bool,
            Option<String>,
            Vec<crate::agent::registry::ImageAttachment>,
        );
        let dt_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<DtItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender_id: String,
                  text: String,
                  conversation_id: String,
                  is_group: bool,
                  images: Vec<crate::agent::registry::ImageAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = cfg.clone();
                let tx = out_tx.clone();
                let tq = Arc::clone(&tq);
                let bound = bound.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&dt_user_queues);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("dingtalk group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == conversation_id) {
                                    debug!(
                                        "dingtalk group message rejected: not in groupAllowFrom"
                                    );
                                    return;
                                }
                            }
                            crate::config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_group {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&sender_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %sender_id, "dingtalk DM rejected by policy");
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
                            let (utx, mut urx) = mpsc::channel::<DtItem>(32);
                            map.insert(sender_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = cfg.clone();
                            let w_uid = sender_id.clone();
                            let w_tq = Arc::clone(&tq);
                            tokio::spawn(async move {
                                while let Some((
                                    text,
                                    sender_id,
                                    conversation_id,
                                    is_group,
                                    bound,
                                    images,
                                )) = urx.recv().await
                                {
                                    // No debounce — task queue merge_into_pending
                                    // handles rapid consecutive messages automatically.
                                    let handle = if let Some(ref agent_id) = bound {
                                        match w_reg.get(agent_id) {
                                            Ok(h) => h,
                                            Err(_) => match w_reg.route_account("dingtalk", None) {
                                                Ok(h) => h,
                                                Err(e) => { error!("dingtalk route error: {e:#}"); continue; }
                                            },
                                        }
                                    } else {
                                        match w_reg.route_account("dingtalk", None) {
                                            Ok(h) => h,
                                            Err(e) => { error!("dingtalk route error: {e:#}"); continue; }
                                        }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: if is_group {
                                            MessageKind::GroupMessage {
                                                group_id: conversation_id.clone(),
                                                thread_id: None,
                                            }
                                        } else {
                                            MessageKind::DirectMessage { account_id: None }
                                        },
                                        channel: "dingtalk".to_string(),
                                        peer_id: sender_id.clone(),
                                        dm_scope,
                                    });
                                    let dt_target = if is_group { conversation_id.clone() } else { sender_id.clone() };
                                    let qmsg = crate::gateway::task_queue::QueuedMessage {
                                        text,
                                        sender: sender_id.to_string(),
                                        channel: "dingtalk".to_string(),
                                        chat_id: dt_target.clone(),
                                        is_group,
                                        timestamp: chrono::Utc::now().timestamp(),
                                        images: images.iter().map(|i| i.data.clone()).collect(),
                                        files: vec![],
                                    };
                                    if let Err(e) = w_tq.submit(&session_key, qmsg, crate::gateway::task_queue::Priority::User) {
                                        error!(user = %w_uid, "dingtalk: queue submit failed: {e:#}");
                                    }
                                }
                                debug!(user = %w_uid, "dingtalk: per-user worker stopped");
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
                        let cfg = cfg.clone();
                        let question = text[5..].to_owned();
                        let sender_id = sender_id.clone();
                        let conversation_id = conversation_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route_account("dingtalk", None) {
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
                                let target = if is_group { conversation_id } else { sender_id };
                                if let Err(e) = tx
                                    .send(OutboundMessage {
                                        target_id: target,
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
                        let sender_id = sender_id.clone();
                        let conversation_id = conversation_id.clone();
                        let bound = bound.clone();
                        tokio::spawn(async move {
                            let handle = if let Some(ref agent_id) = bound {
                                match reg.get(agent_id) {
                                    Ok(h) => h,
                                    Err(_) => match reg.route_account("dingtalk", None) {
                                        Ok(h) => h,
                                        Err(_) => return,
                                    },
                                }
                            } else {
                                match reg.route_account("dingtalk", None) {
                                    Ok(h) => h,
                                    Err(_) => return,
                                }
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: conversation_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "dingtalk".to_string(),
                                peer_id: sender_id.clone(),
                                dm_scope,
                            });
                            if let Some(mut reply) = try_preparse_locally(&text, &handle).await {
                                reply.target_id = if is_group { conversation_id.clone() } else { sender_id.clone() };
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
                                channel: "dingtalk".to_string(),
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
                                    let target = if is_group { conversation_id } else { sender_id };
                                    if let Err(e) = tx.send(OutboundMessage {
                                        target_id: target,
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
                    if let Err(e) = user_tx.try_send((
                        text,
                        sender_id.clone(),
                        conversation_id,
                        is_group,
                        bound,
                        images,
                    )) {
                        warn!(user = %sender_id, error = %e, "dingtalk: user queue full, dropping message");
                    }
                });
            },
        );

        let dt = Arc::new(crate::channel::dingtalk::DingTalkChannel::new(
            app_key,
            app_secret,
            robot_code,
            dt_cfg.api_base.clone(),
            dt_cfg.oapi_base.clone(),
            on_message,
        ));
        if let Err(e) = manager.register(Arc::clone(&dt) as Arc<dyn crate::channel::Channel>) {
            tracing::warn!("failed to register channel: {e}");
        }
        let dt_send = Arc::clone(&dt);

        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = dt_send.send(msg).await {
                    error!("dingtalk send error: {e:#}");
                }
            }
        });

        tokio::spawn(async move {
            if let Err(e) = dt.run().await {
                error!("dingtalk channel error: {e:#}");
            }
        });

        info!(account = %acct_for_log, "dingtalk channel started");
    }
}
