//! LiveConfig — per-domain Arc<RwLock<T>> wrappers for hot-reloadable config.
//!
//! Each sub-struct maps to one hot-reload domain so subscribers can update
//! only the slices they care about without taking a global lock.
//!
//! Usage in AppState:
//!   `state.live.gateway.read().auth_token`  — reads the currently active token
//!
//! On config file change:
//!   `live.apply(new_cfg, &restart_tx)`      — diffs and updates each lock

use std::sync::Arc;

use tokio::sync::{RwLock, broadcast};
use tracing::{info, warn};

use crate::config::runtime::{
    AgentsRuntime, ChannelRuntime, ExtRuntime, GatewayRuntime, ModelRuntime, OpsRuntime,
    RuntimeConfig,
};

// ---------------------------------------------------------------------------
// LiveConfig
// ---------------------------------------------------------------------------

/// Per-domain live handles.  Each field can be independently read-locked
/// by request handlers and written during a hot-reload.
#[derive(Clone)]
pub struct LiveConfig {
    pub gateway: Arc<RwLock<GatewayRuntime>>,
    pub agents: Arc<RwLock<AgentsRuntime>>,
    pub channel: Arc<RwLock<ChannelRuntime>>,
    pub model: Arc<RwLock<ModelRuntime>>,
    pub ext: Arc<RwLock<ExtRuntime>>,
    pub ops: Arc<RwLock<OpsRuntime>>,
}

impl LiveConfig {
    /// Wrap an initial `RuntimeConfig`.
    pub fn new(cfg: RuntimeConfig) -> Self {
        Self {
            gateway: Arc::new(RwLock::new(cfg.gateway)),
            agents: Arc::new(RwLock::new(cfg.agents)),
            channel: Arc::new(RwLock::new(cfg.channel)),
            model: Arc::new(RwLock::new(cfg.model)),
            ext: Arc::new(RwLock::new(cfg.ext)),
            ops: Arc::new(RwLock::new(cfg.ops)),
        }
    }

    /// Apply a freshly-loaded config.
    ///
    /// Fields in `RESTART_FIELDS` that changed are collected and emitted via
    /// `restart_tx`.  All other domains are updated in-place.
    ///
    /// Returns the list of fields that require a restart (empty = clean
    /// reload).
    pub async fn apply(
        &self,
        new: RuntimeConfig,
        restart_tx: &broadcast::Sender<Vec<String>>,
    ) -> Vec<String> {
        let mut restart_fields = {
            let old_gw = self.gateway.read().await;
            detect_restart_fields(&old_gw, &new.gateway)
        };

        // Channel changes (add/remove) require restart since channels are
        // long-running tasks spawned at startup.
        {
            let old_ch = self.channel.read().await;
            if channels_changed(&old_ch.channels, &new.channel.channels) {
                restart_fields.push("channels".to_owned());
            }
        }

        // Model/agent changes require restart since AgentRuntime holds a
        // config snapshot from startup.
        {
            let old_agents = self.agents.read().await;
            let old_primary = old_agents.defaults.model.as_ref()
                .and_then(|m| m.primary.as_deref());
            let new_primary = new.agents.defaults.model.as_ref()
                .and_then(|m| m.primary.as_deref());
            if old_primary != new_primary || old_agents.list.len() != new.agents.list.len() {
                restart_fields.push("agents/model".to_owned());
            }
        }

        if !restart_fields.is_empty() {
            warn!(
                ?restart_fields,
                "hot-reload skipped: fields require gateway restart"
            );
            let _ = restart_tx.send(restart_fields.clone());
            return restart_fields;
        }

        // Update all domains.
        *self.gateway.write().await = new.gateway;
        *self.agents.write().await = new.agents;
        *self.channel.write().await = new.channel;
        *self.model.write().await = new.model;
        *self.ext.write().await = new.ext;
        *self.ops.write().await = new.ops;

        info!("hot-reload applied — all domains updated");
        vec![]
    }

    /// Reconstruct a point-in-time `RuntimeConfig` snapshot.
    ///
    /// Useful for backwards-compatible code that still works with the full
    /// struct. Acquires all read locks sequentially (no deadlock risk — no
    /// write path holds multiple locks simultaneously).
    pub async fn snapshot(&self) -> RuntimeConfig {
        RuntimeConfig {
            gateway: self.gateway.read().await.clone(),
            agents: self.agents.read().await.clone(),
            channel: self.channel.read().await.clone(),
            model: self.model.read().await.clone(),
            ext: self.ext.read().await.clone(),
            ops: self.ops.read().await.clone(),
            raw: Default::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Restart-required diff
// ---------------------------------------------------------------------------

/// Fields that cannot be changed without restarting the gateway.
pub(crate) fn detect_restart_fields(old: &GatewayRuntime, new: &GatewayRuntime) -> Vec<String> {
    let mut fields = Vec::new();
    if old.port != new.port {
        fields.push("gateway.port".to_owned());
    }
    if old.bind != new.bind {
        fields.push("gateway.bind".to_owned());
    }
    if old.reload != new.reload {
        fields.push("gateway.reload".to_owned());
    }
    fields
}

/// True when the only differences between `old` and `new` are fields that
/// `AgentRuntime` reads live (currently `agents.defaults.temperature` and
/// `agents.list[i].temperature`).
///
/// When this returns true the hot-reload watcher can skip the restart banner —
/// the change has already taken effect via `LiveConfig::apply`. Anything else
/// (model changes, prompt edits, channel adds/removes, …) still snapshots into
/// AgentRuntime fields at startup, so a restart banner is required.
///
/// Implementation note: `RuntimeConfig` doesn't derive `PartialEq`, but its
/// `raw: Config` schema does derive `Serialize`. We serialize both, blank out
/// the live-mutable temperature fields, and compare the JSON values.
pub fn is_hot_safe_only(old: &RuntimeConfig, new: &RuntimeConfig) -> bool {
    let mut old_v = match serde_json::to_value(&old.raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let mut new_v = match serde_json::to_value(&new.raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    strip_live_fields(&mut old_v);
    strip_live_fields(&mut new_v);
    old_v == new_v
}

/// Remove `agents.defaults.temperature` and every `agents.list[i].temperature`
/// from a serialized `Config` so two snapshots can be compared on the fields
/// that actually require restart.
fn strip_live_fields(v: &mut serde_json::Value) {
    let agents = match v.get_mut("agents") {
        Some(a) => a,
        None => return,
    };
    if let Some(defaults) = agents.get_mut("defaults") {
        if let Some(obj) = defaults.as_object_mut() {
            obj.remove("temperature");
        }
    }
    if let Some(list) = agents.get_mut("list").and_then(|l| l.as_array_mut()) {
        for entry in list {
            if let Some(obj) = entry.as_object_mut() {
                obj.remove("temperature");
            }
        }
    }
}

/// Detect if any channel was added or removed (requires restart).
fn channels_changed(
    old: &crate::config::schema::ChannelsConfig,
    new: &crate::config::schema::ChannelsConfig,
) -> bool {
    old.telegram.is_some() != new.telegram.is_some()
        || old.discord.is_some() != new.discord.is_some()
        || old.slack.is_some() != new.slack.is_some()
        || old.whatsapp.is_some() != new.whatsapp.is_some()
        || old.signal.is_some() != new.signal.is_some()
        || old.feishu.is_some() != new.feishu.is_some()
        || old.dingtalk.is_some() != new.dingtalk.is_some()
        || old.wecom.is_some() != new.wecom.is_some()
        || old.wechat.is_some() != new.wechat.is_some()
        || old.qq.is_some() != new.qq.is_some()
        || old.line.is_some() != new.line.is_some()
        || old.zalo.is_some() != new.zalo.is_some()
        || old.matrix.is_some() != new.matrix.is_some()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{BindMode, GatewayMode, ReloadMode};

    fn base_gw() -> GatewayRuntime {
        GatewayRuntime {
            port: 18888,
            mode: GatewayMode::Local,
            bind: BindMode::Loopback,
            bind_address: None,
            reload: ReloadMode::Hybrid,
            auth_token: None,
            allow_tailscale: false,
            channel_health_check_minutes: 5,
            channel_stale_event_threshold_minutes: 30,
            channel_max_restarts_per_hour: 10,
            auth_token_configured: false,
            auth_token_is_plaintext: false,
            user_agent: None,
            language: None,
        }
    }

    #[test]
    fn no_restart_for_auth_token_change() {
        let old = base_gw();
        let mut new = old.clone();
        new.auth_token = Some("new-token".to_owned());
        assert!(detect_restart_fields(&old, &new).is_empty());
    }

    #[test]
    fn restart_required_for_port_change() {
        let old = base_gw();
        let mut new = old.clone();
        new.port = 19000;
        let fields = detect_restart_fields(&old, &new);
        assert!(fields.contains(&"gateway.port".to_owned()));
    }

    #[test]
    fn restart_required_for_bind_change() {
        let old = base_gw();
        let mut new = old.clone();
        new.bind = BindMode::All;
        let fields = detect_restart_fields(&old, &new);
        assert!(fields.contains(&"gateway.bind".to_owned()));
    }

    #[tokio::test]
    async fn apply_updates_auth_token() {
        use crate::config::{
            runtime::{AgentsRuntime, ChannelRuntime, ExtRuntime, ModelRuntime, OpsRuntime},
            schema::SessionConfig,
        };

        let gw = base_gw();
        let cfg = RuntimeConfig {
            gateway: gw,
            agents: AgentsRuntime {
                defaults: Default::default(),
                list: vec![],
                bindings: vec![],
                external: vec![],
            },
            channel: ChannelRuntime {
                channels: Default::default(),
                session: SessionConfig {
                    dm_scope: None,
                    thread_bindings: None,
                    reset: None,
                    identity_links: None,
                    maintenance: None,
                },
            },
            model: ModelRuntime {
                models: None,
                auth: None,
            },
            ext: ExtRuntime {
                tools: None,
                skills: None,
                plugins: None,
            },
            ops: OpsRuntime {
                cron: None,
                hooks: None,
                sandbox: None,
                logging: None,
                secrets: None,
            },
            raw: Default::default(),
        };
        let live = LiveConfig::new(cfg.clone());

        let mut new_cfg = cfg;
        new_cfg.gateway.auth_token = Some("rotated".to_owned());

        let (tx, _) = broadcast::channel(8);
        let restart = live.apply(new_cfg, &tx).await;
        assert!(restart.is_empty());
        assert_eq!(
            live.gateway.read().await.auth_token.as_deref(),
            Some("rotated")
        );
    }

    fn empty_runtime_config() -> RuntimeConfig {
        use crate::config::{
            runtime::{AgentsRuntime, ChannelRuntime, ExtRuntime, ModelRuntime, OpsRuntime},
            schema::SessionConfig,
        };
        RuntimeConfig {
            gateway: base_gw(),
            agents: AgentsRuntime {
                defaults: Default::default(),
                list: vec![],
                bindings: vec![],
                external: vec![],
            },
            channel: ChannelRuntime {
                channels: Default::default(),
                session: SessionConfig {
                    dm_scope: None,
                    thread_bindings: None,
                    reset: None,
                    identity_links: None,
                    maintenance: None,
                },
            },
            model: ModelRuntime {
                models: None,
                auth: None,
            },
            ext: ExtRuntime {
                tools: None,
                skills: None,
                plugins: None,
            },
            ops: OpsRuntime {
                cron: None,
                hooks: None,
                sandbox: None,
                logging: None,
                secrets: None,
            },
            raw: Default::default(),
        }
    }

    #[test]
    fn hot_safe_when_only_default_temperature_changes() {
        let mut old = empty_runtime_config();
        let mut new = empty_runtime_config();
        old.raw.agents = Some(crate::config::schema::AgentsConfig {
            defaults: Some(crate::config::schema::AgentDefaults {
                temperature: Some(0.5),
                ..Default::default()
            }),
            ..Default::default()
        });
        new.raw.agents = Some(crate::config::schema::AgentsConfig {
            defaults: Some(crate::config::schema::AgentDefaults {
                temperature: Some(0.3),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(is_hot_safe_only(&old, &new));
    }

    #[test]
    fn not_hot_safe_when_port_changes() {
        let old = empty_runtime_config();
        let mut new = empty_runtime_config();
        new.raw.gateway = Some(crate::config::schema::GatewayConfig {
            port: Some(19000),
            ..Default::default()
        });
        assert!(!is_hot_safe_only(&old, &new));
    }
}
