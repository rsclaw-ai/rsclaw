use std::{sync::Arc, time::Duration};

use anyhow::anyhow;
use futures::future::BoxFuture;
use rsclaw_agent::{AgentMessage, AgentRegistry};
use rsclaw_channel::{Channel, OutboundMessage, signal::SignalChannel};
use rsclaw_config::runtime::RuntimeConfig;
use tokio::sync::{Notify, OnceCell, mpsc};
use tracing::{debug, error, info, warn};

use super::{
    super::preparse::{btw_direct_call, is_fast_preparse, try_preparse_locally},
    default_dm_scope,
};
use crate::gateway::session::{MessageKind, SessionKeyParams, derive_session_key};

/// How long the proxy will wait for the real `SignalChannel` to finish
/// initializing before erroring out. Covers the signal-cli spawn cost on
/// gateway start (typically <1s).
const SIGNAL_PROXY_READY_WAIT: Duration = Duration::from_secs(5);

/// Forwarding stub registered in `ChannelManager` synchronously at startup;
/// the real `SignalChannel` is spawned asynchronously (signal-cli subprocess)
/// and filled into the `OnceCell` once available. `ready` is pulsed when the
/// slot is filled so `send()` can wait the first few seconds after gateway
/// start without dropping cron deliveries.
struct SignalProxy {
    name: String,
    real: Arc<OnceCell<Arc<SignalChannel>>>,
    ready: Arc<Notify>,
}

impl Channel for SignalProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            if let Some(real) = self.real.get() {
                return real.send(msg).await;
            }
            // Cold path: gateway just started, signal-cli still spawning.
            // Wait briefly for the slot to fill instead of dropping the
            // message. `notified()` is created before the check that follows
            // so a `notify_one` that fires between our get() and notified()
            // is still observed.
            let notified = self.ready.notified();
            if let Some(real) = self.real.get() {
                return real.send(msg).await;
            }
            match tokio::time::timeout(SIGNAL_PROXY_READY_WAIT, notified).await {
                Ok(()) => {}
                Err(_) => {
                    return Err(anyhow!(
                        "signal channel still not ready after {:?}",
                        SIGNAL_PROXY_READY_WAIT
                    ));
                }
            }
            let real = self
                .real
                .get()
                .ok_or_else(|| anyhow!("signal channel ready signaled but slot empty"))?;
            real.send(msg).await
        })
    }

    /// The proxy is a dispatch shim; the real channel's `run` loop is driven
    /// by the task that filled the slot.
    fn run(self: Arc<Self>) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

pub(crate) fn start_signal_if_configured(
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
    let Some(sig_cfg) = &config.channel.channels.signal else {
        return;
    };
    if !sig_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = sig_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(rsclaw_config::schema::DmPolicy::Pairing);
    let group_policy = sig_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(rsclaw_config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = sig_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = sig_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        rsclaw_channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("signal", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("signal".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, phone) pairs from accounts.<name>.phone.
    let mut sig_accounts: Vec<(String, String)> = Vec::new();
    if let Some(accts) = &sig_cfg.accounts {
        for (name, acct) in accts {
            let p = acct.get("phone").and_then(|v| v.as_str()).unwrap_or("");
            if !p.is_empty() {
                sig_accounts.push((name.clone(), p.to_owned()));
            }
        }
    }

    if sig_accounts.is_empty() {
        warn!("signal.phone not set in accounts, channel disabled");
        return;
    }
    let sig_cli_path = sig_cfg.cli_path.clone();

    for (acct_name, phone) in sig_accounts {
        let acct_for_log = acct_name.clone();
        let sig_acct_outer = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let sig_cli_path = sig_cli_path.clone();
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register Signal channel sender for notification routing.
        // - "signal/{account}" is the canonical key for multi-account routing.
        // - bare "signal" registered only by the first account so legacy callers still
        //   find a sender. Without first-wins guarding, each account would overwrite
        //   the bare key and replies route via the wrong principal.
        {
            let mut senders = channel_senders
                .write()
                .expect("channel_senders lock poisoned");
            senders.insert(format!("signal/{}", acct_name), out_tx.clone());
            senders
                .entry("signal".to_string())
                .or_insert_with(|| out_tx.clone());
        }

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());
        let tq = Arc::clone(&task_queue);

        // Per-user inbound queue for Signal.
        type SigItem = (String, String, bool);
        let sig_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<SigItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(move |sender: String, text: String, is_group: bool| {
            let reg = Arc::clone(&reg);
            let cfg = Arc::clone(&cfg_arc);
            let tx = out_tx.clone();
            let enforcer = Arc::clone(&enforcer);
            let group_policy = Arc::clone(&gp);
            let group_allow = Arc::clone(&ga);
            let queues = Arc::clone(&sig_user_queues);
            let tq = Arc::clone(&tq);
            let sig_acct = sig_acct_outer.clone();
            tokio::spawn(async move {
                // Group policy check.
                if is_group {
                    match group_policy.as_ref() {
                        rsclaw_config::schema::GroupPolicy::Disabled => {
                            warn!("signal group message rejected: groupPolicy=disabled");
                            return;
                        }
                        rsclaw_config::schema::GroupPolicy::Allowlist => {
                            if !group_allow.iter().any(|g| *g == sender) {
                                warn!("signal group message rejected: not in groupAllowFrom");
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
                            warn!(peer_id = %sender, "signal DM rejected by policy");
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
                                    ),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,

                                    account: Some(sig_acct.clone()),
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

                                    account: Some(sig_acct.clone()),
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
                        let (utx, mut urx) = mpsc::channel::<SigItem>(32);
                        map.insert(sender.clone(), utx.clone());
                        let w_reg = Arc::clone(&reg);
                        let w_cfg = Arc::clone(&cfg);
                        let w_uid = sender.clone();
                        let w_tq = Arc::clone(&tq);
                        let w_acct = sig_acct.clone();
                        tokio::spawn(async move {
                            while let Some((text, sender, is_group)) = urx.recv().await {
                                // No debounce — task queue merge_into_pending
                                // handles rapid consecutive messages automatically.
                                let handle = match w_reg
                                    .route_account("signal", Some(&w_acct))
                                    .or_else(|_| w_reg.route_account("signal", None))
                                    .or_else(|_| w_reg.default_agent())
                                {
                                    Ok(h) => h,
                                    Err(e) => {
                                        error!("signal route: {e:#}");
                                        continue;
                                    }
                                };
                                let dm_scope = default_dm_scope(&w_cfg);
                                let session_key = derive_session_key(&SessionKeyParams {
                                    agent_id: handle.id.clone(),
                                    kind: if is_group {
                                        MessageKind::GroupMessage {
                                            group_id: sender.clone(),
                                            thread_id: None,
                                        }
                                    } else {
                                        MessageKind::DirectMessage {
                                            account_id: Some(w_acct.clone()),
                                        }
                                    },
                                    channel: "signal".to_string(),
                                    peer_id: sender.clone(),
                                    dm_scope,
                                });
                                let qmsg = crate::gateway::task_queue::QueuedMessage {
                                    text,
                                    sender: sender.clone(),
                                    channel: "signal".to_string(),
                                    chat_id: String::new(),
                                    is_group,
                                    reply_to: None,
                                    timestamp: chrono::Utc::now().timestamp(),
                                    images: vec![],
                                    files: vec![],
                                    account: Some(w_acct.clone()),
                                };
                                if let Err(e) = w_tq.submit(
                                    &session_key,
                                    qmsg,
                                    crate::gateway::task_queue::Priority::User,
                                ) {
                                    error!(user = %w_uid, "signal: queue submit failed: {e:#}");
                                }
                            }
                            debug!(user = %w_uid, "signal: per-user worker stopped");
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
                    let cfg = Arc::clone(&cfg);
                    let question = text[5..].to_owned();
                    let sender = sender.clone();
                    let sig_acct = sig_acct.clone();
                    tokio::spawn(async move {
                        let handle = match reg
                            .route_account("signal", Some(&sig_acct))
                            .or_else(|_| reg.route_account("signal", None))
                            .or_else(|_| reg.default_agent())
                        {
                            Ok(h) => h,
                            Err(_) => return,
                        };
                        if let Some(reply_text) =
                            btw_direct_call(&question, &handle.live_status, &handle.providers, &cfg)
                                .await
                        {
                            if let Err(e) = tx
                                .send(OutboundMessage {
                                    target_id: sender,
                                    is_group: false,
                                    text: format!("[/btw] {}", reply_text),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,

                                    account: Some(sig_acct),
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
                    let sender = sender.clone();
                    let sig_acct = sig_acct.clone();
                    tokio::spawn(async move {
                        let handle = match reg
                            .route_account("signal", Some(&sig_acct))
                            .or_else(|_| reg.route_account("signal", None))
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
                                    group_id: sender.clone(),
                                    thread_id: None,
                                }
                            } else {
                                MessageKind::DirectMessage {
                                    account_id: Some(sig_acct.clone()),
                                }
                            },
                            channel: "signal".to_string(),
                            peer_id: sender.clone(),
                            dm_scope,
                        });
                        if let Some(mut reply) = try_preparse_locally(
                            &text,
                            &handle,
                            "signal",
                            &sender,
                            crate::gateway::preparse::PreparseOrigin::User,
                        )
                        .await
                        {
                            reply.target_id = sender.clone();
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
                            channel: "signal".to_string(),
                            peer_id: sender.clone(),
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
                            account: Some(sig_acct.clone()),
                        };
                        if handle.tx.send(msg).await.is_err() {
                            return;
                        }
                        match tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx)
                            .await
                        {
                            Ok(Ok(r)) => {
                                if !r.is_empty {
                                    if let Err(e) = tx
                                        .send(OutboundMessage {
                                            target_id: sender,
                                            is_group,
                                            text: r.text,
                                            reply_to: None,
                                            images: r.images,
                                            files: r.files,
                                            channel: None,
                                            account: Some(sig_acct),
                                        })
                                        .await
                                    {
                                        tracing::warn!("failed to send message: {e}");
                                    }
                                }
                            }
                            Ok(Err(_)) => {
                                warn!("signal: chat-mode agent reply error");
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: sender.clone(),
                                        is_group,
                                        text: rsclaw_i18n::t(
                                            "chat_reply_error",
                                            rsclaw_i18n::default_lang(),
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        files: vec![],
                                        channel: None,
                                        account: Some(sig_acct.clone()),
                                    })
                                    .await;
                            }
                            Err(_) => {
                                warn!("signal: chat-mode agent reply timed out");
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: sender.clone(),
                                        is_group,
                                        text: rsclaw_i18n::t(
                                            "chat_reply_timeout",
                                            rsclaw_i18n::default_lang(),
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        files: vec![],
                                        channel: None,
                                        account: Some(sig_acct.clone()),
                                    })
                                    .await;
                            }
                        }
                    });
                    return;
                }
                if let Err(e) = user_tx.try_send((text, sender.clone(), is_group)) {
                    warn!(user = %sender, error = %e, "signal: user queue full, dropping message");
                }
            });
        });

        // Register a synchronous proxy in ChannelManager so cron/watch can
        // resolve "signal/{acct}" (and bare "signal" for the first account)
        // even though the real SignalChannel is spawned asynchronously below.
        // The proxy forwards `send` to the OnceCell once filled, briefly
        // waiting (via `ready` Notify) for the cold-start window so an early
        // cron tick doesn't lose its message.
        let signal_slot: Arc<OnceCell<Arc<SignalChannel>>> = Arc::new(OnceCell::new());
        let signal_ready = Arc::new(Notify::new());
        let proxy_name = format!("signal/{}", acct_name);
        let proxy = Arc::new(SignalProxy {
            name: proxy_name.clone(),
            real: Arc::clone(&signal_slot),
            ready: Arc::clone(&signal_ready),
        });
        if let Err(e) =
            manager.register_with_name(proxy_name.clone(), Arc::clone(&proxy) as Arc<dyn Channel>)
        {
            warn!(account = %acct_for_log, "signal: failed to register proxy: {e:#}");
        }
        if manager.get("signal").is_none() {
            let bare = Arc::new(SignalProxy {
                name: "signal".to_owned(),
                real: Arc::clone(&signal_slot),
                ready: Arc::clone(&signal_ready),
            });
            if let Err(e) =
                manager.register_with_name("signal".to_owned(), bare as Arc<dyn Channel>)
            {
                warn!("signal: failed to register bare proxy: {e:#}");
            }
        }

        // spawn() is async — drive it in a task. Once SignalChannel is constructed,
        // fill the slot and pulse `ready` so the proxy can release any waiting
        // sends.
        let shutdown_for_signal = shutdown.clone();
        let cancel_token = manager.register_cancel_token(&proxy_name);
        tokio::spawn(async move {
            match SignalChannel::spawn(phone, sig_cli_path, on_message).await {
                Ok(ch) => {
                    let ch = Arc::new(ch);
                    if signal_slot.set(Arc::clone(&ch)).is_err() {
                        warn!(account = %acct_for_log, "signal: slot already filled (duplicate spawn?)");
                    }
                    signal_ready.notify_waiters();
                    let ch_send = Arc::clone(&ch);
                    let shutdown_for_out = shutdown_for_signal.clone();
                    let cancel_for_out = cancel_token.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                () = shutdown_for_out.notified() => {
                                    info!("signal: drain signaled, stopping outbound sender");
                                    break;
                                }
                                () = cancel_for_out.cancelled() => {
                                    info!("signal: channel cancelled, stopping outbound sender");
                                    break;
                                }
                                msg = out_rx.recv() => {
                                    let Some(msg) = msg else { break };
                                    if let Err(e) = ch_send.send(msg).await {
                                        error!("signal send: {e:#}");
                                    }
                                }
                            }
                        }
                    });
                    info!(account = %acct_for_log, "signal channel started");
                    tokio::select! {
                        res = ch.run() => {
                            if let Err(e) = res {
                                error!("signal channel: {e:#}");
                            }
                        }
                        () = shutdown_for_signal.notified() => {
                            info!("signal: drain signaled, stopping run loop");
                        }
                        () = cancel_token.cancelled() => {
                            info!("signal: channel cancelled, stopping run loop");
                        }
                    }
                }
                Err(e) => warn!("signal-cli not available: {e:#}"),
            }
        });
    } // end for sig_accounts
}
