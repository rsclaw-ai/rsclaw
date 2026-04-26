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

use crate::config::{
    runtime::{
        AgentsRuntime, ChannelRuntime, ExtRuntime, GatewayRuntime, ModelRuntime, OpsRuntime,
        RuntimeConfig,
    },
    schema::Config,
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
    /// Original parsed `Config` retained so `snapshot()` can return the same
    /// `RuntimeConfig` shape that loader produced. Required for
    /// `is_hot_safe_only` / `diff_restart_sections` which compare via the raw
    /// JSON tree.
    pub raw: Arc<RwLock<Config>>,
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
            raw: Arc::new(RwLock::new(cfg.raw)),
        }
    }

    /// Apply a freshly-loaded config.
    ///
    /// Behaviour:
    ///   - If the change is hot-safe (only fields whitelisted in
    ///     `strip_live_fields` differ — currently per-agent / default
    ///     temperature) the new values are written into all live RwLocks and
    ///     an empty list is returned.
    ///   - Otherwise the current live state is left untouched, the changed
    ///     section names are returned (and broadcast via `restart_tx`), and
    ///     the caller is expected to surface a restart banner.
    ///
    /// `is_hot_safe_only` / `diff_restart_sections` is the single source of
    /// truth — coarse field-by-field checks were removed in favour of a
    /// JSON-equality diff over `raw`.
    pub async fn apply(
        &self,
        new: RuntimeConfig,
        restart_tx: &broadcast::Sender<Vec<String>>,
    ) -> Vec<String> {
        let old = self.snapshot().await;
        let restart_fields = diff_restart_sections(&old, &new);

        if !restart_fields.is_empty() {
            warn!(
                ?restart_fields,
                "hot-reload skipped: fields require gateway restart"
            );
            if restart_tx.send(restart_fields.clone()).is_err() {
                tracing::debug!("restart broadcast: no receivers");
            }
            return restart_fields;
        }

        // Hot-safe — write new values into every live lock so subscribers
        // (e.g. AgentRuntime reading temperature) pick up the change.
        *self.gateway.write().await = new.gateway;
        *self.agents.write().await = new.agents;
        *self.channel.write().await = new.channel;
        *self.model.write().await = new.model;
        *self.ext.write().await = new.ext;
        *self.ops.write().await = new.ops;
        *self.raw.write().await = new.raw;

        info!("hot-reload applied — all domains updated");
        vec![]
    }

    /// Reconstruct a point-in-time `RuntimeConfig` snapshot.
    ///
    /// Acquires all read locks sequentially (no deadlock risk — no write path
    /// holds multiple locks simultaneously). `raw` is preserved so callers can
    /// pass the snapshot to `is_hot_safe_only` / `diff_restart_sections`.
    pub async fn snapshot(&self) -> RuntimeConfig {
        RuntimeConfig {
            gateway: self.gateway.read().await.clone(),
            agents: self.agents.read().await.clone(),
            channel: self.channel.read().await.clone(),
            model: self.model.read().await.clone(),
            ext: self.ext.read().await.clone(),
            ops: self.ops.read().await.clone(),
            raw: self.raw.read().await.clone(),
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

/// True when every difference between `old` and `new` is in the hot-safe
/// whitelist (currently `agents.defaults.temperature` and per-agent
/// `agents.list[i].temperature`).
///
/// When true, the hot-reload watcher can skip the restart banner — the change
/// is fully applied by `LiveConfig::apply`. Anything else (model changes,
/// prompt edits, channel/account adds/removes, binding changes, …) still gets
/// snapshotted into running tasks at startup, so a restart banner is required.
pub fn is_hot_safe_only(old: &RuntimeConfig, new: &RuntimeConfig) -> bool {
    diff_restart_sections(old, new).is_empty()
}

/// Compute the list of section names that changed in non-hot-safe ways.
///
/// Empty list = the change was fully hot-safe (caller can apply silently).
/// Non-empty = caller must surface a restart banner; the entries are suitable
/// for display, e.g. `["gateway.port", "config.channels"]`.
///
/// Implementation:
///   1. Granular gateway fields (port/bind/reload) come from
///      `detect_restart_fields`.
///   2. Top-level `Config` keys are compared via JSON equality on `raw`.
///      Hot-safe fields are stripped before comparison so a pure temperature
///      tweak yields no entry. Fields already covered by
///      `detect_restart_fields` are also stripped so the diff doesn't
///      double-report them as a coarse `config.gateway` entry.
pub fn diff_restart_sections(old: &RuntimeConfig, new: &RuntimeConfig) -> Vec<String> {
    let mut sections = detect_restart_fields(&old.gateway, &new.gateway);

    let mut old_v = serde_json::to_value(&old.raw).unwrap_or(serde_json::Value::Null);
    let mut new_v = serde_json::to_value(&new.raw).unwrap_or(serde_json::Value::Null);
    strip_live_fields(&mut old_v);
    strip_live_fields(&mut new_v);

    // Collect every key present in either object so add/remove of a top-level
    // section is detected even if only one side has it.
    let mut keys: std::collections::BTreeSet<String> = Default::default();
    if let Some(obj) = old_v.as_object() {
        keys.extend(obj.keys().cloned());
    }
    if let Some(obj) = new_v.as_object() {
        keys.extend(obj.keys().cloned());
    }

    for key in keys {
        let old_val = old_v.get(&key);
        let new_val = new_v.get(&key);
        if old_val != new_val {
            sections.push(format!("config.{key}"));
        }
    }
    sections
}

/// Strip every field that is either:
///   * already reported granularly by `detect_restart_fields`
///     (gateway.port / gateway.bind / gateway.bindAddress / gateway.reload),
///     so the JSON diff doesn't re-emit a coarse `config.gateway` entry; or
///   * actually read live by request handlers and therefore safe to change
///     without restart (`gateway.auth.token`,
///     `agents.defaults.temperature`, every `agents.list[i].temperature`).
///
/// Fields not listed here remain in the diff, so changing them surfaces a
/// restart banner — that is the conservative default for anything we haven't
/// audited yet.
fn strip_live_fields(v: &mut serde_json::Value) {
    if let Some(gateway) = v.get_mut("gateway") {
        if let Some(obj) = gateway.as_object_mut() {
            // Covered by detect_restart_fields — stripping prevents
            // `config.gateway` from appearing alongside `gateway.port` etc.
            obj.remove("port");
            obj.remove("bind");
            obj.remove("bindAddress");
            obj.remove("reload");
            // gateway.auth.token is read live via
            // `state.live.gateway.auth_token`. Drop the whole auth block
            // afterwards if only the token was inside.
            if let Some(auth) = obj.get_mut("auth") {
                if let Some(auth_obj) = auth.as_object_mut() {
                    auth_obj.remove("token");
                    if auth_obj.is_empty() {
                        *auth = serde_json::Value::Null;
                    }
                }
            }
        }
    }

    if let Some(agents) = v.get_mut("agents") {
        if let Some(defaults) = agents.get_mut("defaults") {
            if let Some(obj) = defaults.as_object_mut() {
                // Whitelisted because every read site has been migrated to
                // `self.live.agents.read().await.defaults.*` — see runtime.rs
                // (and compaction.rs for compaction/context_tokens/
                // kv_cache_mode). Tweaking these in the config file no longer
                // requires a restart.
                for key in [
                    "temperature",
                    "btwTokens",
                    "compaction",
                    "contextPruning",
                    "contextTokens",
                    "frequencyPenalty",
                    "intermediateOutput",
                    "kvCacheMode",
                    "maxIterations",
                    "stripThinkTags",
                    "thinking",
                ] {
                    obj.remove(key);
                }
            }
        }
        if let Some(list) = agents.get_mut("list").and_then(|l| l.as_array_mut()) {
            for entry in list {
                if let Some(obj) = entry.as_object_mut() {
                    // Per-agent overrides — only `temperature` exists on
                    // AgentEntry, the other defaults aren't overridable
                    // per-agent.
                    obj.remove("temperature");
                }
            }
        }
    }

    // tools.* — fields whose every read site has been migrated to
    // `self.live.ext.read().await.tools.*` (tools_web.rs + runtime.rs).
    // Note: the JSON path is top-level `tools`, not `ext.tools` — the
    // `ExtRuntime` struct is just an organisational grouping inside
    // `RuntimeConfig`, while the raw `Config` keeps `tools` flat.
    //
    // `exec` and `upload` are intentionally *not* whitelisted:
    //   - `upload` is read cold by the feishu channel at startup, so even if
    //     the agent runtime hot-reads it, a restart is required for channels.
    //   - `exec` migration was deferred — its read sites still go through
    //     `self.config.ext.tools.exec`, so changing it must trigger a restart.
    if let Some(tools) = v.get_mut("tools") {
        if let Some(obj) = tools.as_object_mut() {
            for key in [
                "webSearch",
                "webFetch",
                "webBrowser",
                "loopDetection",
                "sessionResultLimits",
            ] {
                obj.remove(key);
            }
        }
    }

    // Normalise: recursively drop null and empty-object fields.
    //
    // Without this, `Default::default()`'s unset Options serialise as `null`
    // (e.g. `mode: null, bind: null, ...` inside `gateway`). After stripping
    // the live-read fields, the lingering nulls would still differ between
    // `gateway: null` and `gateway: {mode: null, …}` and trip a
    // `config.gateway` diff even though the only real change is already
    // surfaced as `gateway.port`.
    normalise_empties(v);
}

/// Drop every `null` field and empty object recursively. Keeps the diff
/// focused on substantive changes — additions/removals of actual values.
fn normalise_empties(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for val in map.values_mut() {
                normalise_empties(val);
            }
            map.retain(|_, val| match val {
                serde_json::Value::Null => false,
                serde_json::Value::Object(m) => !m.is_empty(),
                _ => true,
            });
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                normalise_empties(item);
            }
        }
        _ => {}
    }
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
        // Production loader updates both runtime + raw; mirror that.
        new.gateway.port = 19000;
        new.raw.gateway = Some(crate::config::schema::GatewayConfig {
            port: Some(19000),
            ..Default::default()
        });
        let sections = diff_restart_sections(&old, &new);
        assert!(sections.contains(&"gateway.port".to_owned()));
        assert!(!is_hot_safe_only(&old, &new));
    }

    #[test]
    fn not_hot_safe_when_account_added_to_existing_channel() {
        // Regression: previously `channels_changed` only checked is_some()
        // flips per channel, so adding feishu-b to an existing feishu config
        // (still Some) silently passed and no banner fired.
        use crate::config::schema::{ChannelsConfig, FeishuConfig};

        let mut old = empty_runtime_config();
        let mut new = empty_runtime_config();

        let mut feishu_old = FeishuConfig {
            base: Default::default(),
            app_id: None,
            app_secret: None,
            verification_token: None,
            encrypt_key: None,
            streaming: None,
            brand: None,
            api_base: None,
            ws_url: None,
            accounts: Some(Default::default()),
        };
        if let Some(map) = feishu_old.accounts.as_mut() {
            map.insert("feishu-a".to_owned(), serde_json::json!({"appId": "a"}));
        }
        let mut feishu_new = feishu_old.clone();
        if let Some(map) = feishu_new.accounts.as_mut() {
            map.insert("feishu-b".to_owned(), serde_json::json!({"appId": "b"}));
        }

        old.raw.channels = Some(ChannelsConfig {
            feishu: Some(feishu_old),
            ..Default::default()
        });
        new.raw.channels = Some(ChannelsConfig {
            feishu: Some(feishu_new),
            ..Default::default()
        });

        let sections = diff_restart_sections(&old, &new);
        assert!(
            sections.iter().any(|s| s == "config.channels"),
            "expected config.channels in sections, got {sections:?}"
        );
        assert!(!is_hot_safe_only(&old, &new));
    }

    #[test]
    fn not_hot_safe_when_channel_block_removed() {
        use crate::config::schema::{ChannelsConfig, FeishuConfig};

        let mut old = empty_runtime_config();
        let new = empty_runtime_config();

        old.raw.channels = Some(ChannelsConfig {
            feishu: Some(FeishuConfig {
                base: Default::default(),
                app_id: Some("x".to_owned()),
                app_secret: None,
                verification_token: None,
                encrypt_key: None,
                streaming: None,
                brand: None,
                api_base: None,
                ws_url: None,
                accounts: None,
            }),
            ..Default::default()
        });

        let sections = diff_restart_sections(&old, &new);
        assert!(
            sections.iter().any(|s| s == "config.channels"),
            "expected config.channels in sections, got {sections:?}"
        );
    }

    #[tokio::test]
    async fn apply_returns_sections_when_account_added() {
        // End-to-end of the user's bug: feishu had 1 account, file edit adds
        // a second account. `apply` must return non-empty sections so the
        // caller surfaces a restart banner; the previous coarse
        // `channels_changed` (is_some flips only) silently passed.
        use crate::config::schema::{ChannelsConfig, FeishuConfig};

        let mut cfg = empty_runtime_config();
        let mut feishu = FeishuConfig {
            base: Default::default(),
            app_id: None,
            app_secret: None,
            verification_token: None,
            encrypt_key: None,
            streaming: None,
            brand: None,
            api_base: None,
            ws_url: None,
            accounts: Some(Default::default()),
        };
        if let Some(map) = feishu.accounts.as_mut() {
            map.insert("feishu-a".to_owned(), serde_json::json!({"appId": "a"}));
        }
        cfg.raw.channels = Some(ChannelsConfig {
            feishu: Some(feishu.clone()),
            ..Default::default()
        });

        let live = LiveConfig::new(cfg.clone());

        // Build new_cfg with feishu-b added.
        let mut new_cfg = cfg;
        if let Some(channels) = new_cfg.raw.channels.as_mut() {
            if let Some(fs) = channels.feishu.as_mut() {
                if let Some(map) = fs.accounts.as_mut() {
                    map.insert("feishu-b".to_owned(), serde_json::json!({"appId": "b"}));
                }
            }
        }

        let (tx, _) = broadcast::channel(8);
        let sections = live.apply(new_cfg, &tx).await;
        assert!(
            sections.iter().any(|s| s == "config.channels"),
            "expected config.channels in returned sections, got {sections:?}"
        );
    }

    #[test]
    fn hot_safe_when_only_auth_token_changes() {
        // gateway.auth.token is read via state.live.gateway.auth_token by
        // every request handler; rotating it should not bother the user with
        // a restart banner.
        let mut old = empty_runtime_config();
        let mut new = empty_runtime_config();
        old.raw.gateway = Some(crate::config::schema::GatewayConfig {
            auth: Some(crate::config::schema::GatewayAuth {
                mode: None,
                password: None,
                allow_tailscale: None,
                allow_local: None,
                token: Some(crate::config::schema::SecretOrString::Plain("old".into())),
            }),
            ..Default::default()
        });
        new.raw.gateway = Some(crate::config::schema::GatewayConfig {
            auth: Some(crate::config::schema::GatewayAuth {
                mode: None,
                password: None,
                allow_tailscale: None,
                allow_local: None,
                token: Some(crate::config::schema::SecretOrString::Plain("rotated".into())),
            }),
            ..Default::default()
        });
        let sections = diff_restart_sections(&old, &new);
        assert!(sections.is_empty(), "expected no banner sections, got {sections:?}");
        assert!(is_hot_safe_only(&old, &new));
    }

    #[test]
    fn not_hot_safe_when_gateway_language_changes() {
        // Regression: previously diff_restart_sections skipped the entire
        // gateway block, so `language` / `userAgent` / `channelHealth*` etc.
        // silently passed even though they're cold (read at startup only).
        let mut old = empty_runtime_config();
        let mut new = empty_runtime_config();
        old.raw.gateway = Some(crate::config::schema::GatewayConfig {
            language: Some("en".into()),
            ..Default::default()
        });
        new.raw.gateway = Some(crate::config::schema::GatewayConfig {
            language: Some("zh".into()),
            ..Default::default()
        });
        let sections = diff_restart_sections(&old, &new);
        assert!(
            sections.iter().any(|s| s == "config.gateway"),
            "expected config.gateway in sections, got {sections:?}"
        );
    }

    #[test]
    fn port_change_does_not_double_report() {
        // detect_restart_fields surfaces "gateway.port"; the JSON diff path
        // must not also emit "config.gateway" for the same change.
        let old = empty_runtime_config();
        let mut new = empty_runtime_config();
        new.gateway.port = 19000;
        new.raw.gateway = Some(crate::config::schema::GatewayConfig {
            port: Some(19000),
            ..Default::default()
        });
        let sections = diff_restart_sections(&old, &new);
        assert!(sections.contains(&"gateway.port".to_owned()));
        assert!(
            !sections.iter().any(|s| s == "config.gateway"),
            "expected no duplicate config.gateway, got {sections:?}"
        );
    }

    #[tokio::test]
    async fn snapshot_preserves_raw() {
        let mut cfg = empty_runtime_config();
        cfg.raw.agents = Some(crate::config::schema::AgentsConfig {
            defaults: Some(crate::config::schema::AgentDefaults {
                temperature: Some(0.42),
                ..Default::default()
            }),
            ..Default::default()
        });
        let live = LiveConfig::new(cfg.clone());
        let snap = live.snapshot().await;
        // Without raw preservation `is_hot_safe_only` would compare an empty
        // default against the file-loaded raw and always trip the banner.
        assert!(is_hot_safe_only(&snap, &cfg));
    }

    #[test]
    fn hot_safe_when_only_whitelisted_defaults_change() {
        // Every field touched here is read live via
        // `self.live.agents.read().await.defaults.*` (runtime.rs +
        // compaction.rs), so changing them must not fire the restart banner.
        let mut old = empty_runtime_config();
        let mut new = empty_runtime_config();
        old.raw.agents = Some(crate::config::schema::AgentsConfig {
            defaults: Some(crate::config::schema::AgentDefaults {
                temperature: Some(0.5),
                btw_tokens: Some(8_000),
                context_tokens: Some(64_000),
                kv_cache_mode: Some(1),
                max_iterations: Some(20),
                strip_think_tags: Some(true),
                intermediate_output: Some(true),
                frequency_penalty: Some(0.0),
                ..Default::default()
            }),
            ..Default::default()
        });
        new.raw.agents = Some(crate::config::schema::AgentsConfig {
            defaults: Some(crate::config::schema::AgentDefaults {
                temperature: Some(0.7),
                btw_tokens: Some(12_000),
                context_tokens: Some(96_000),
                kv_cache_mode: Some(2),
                max_iterations: Some(30),
                strip_think_tags: Some(false),
                intermediate_output: Some(false),
                frequency_penalty: Some(0.1),
                ..Default::default()
            }),
            ..Default::default()
        });
        let sections = diff_restart_sections(&old, &new);
        assert!(
            sections.is_empty(),
            "expected no banner, got {sections:?}"
        );
        assert!(is_hot_safe_only(&old, &new));
    }

    #[test]
    fn not_hot_safe_when_non_whitelisted_default_changes() {
        // `workspace` is still read cold from `self.config.agents.defaults`
        // by tools_file.rs / tools_acp.rs / etc., so changing it must
        // surface a restart banner until those reads are migrated too.
        let mut old = empty_runtime_config();
        let mut new = empty_runtime_config();
        old.raw.agents = Some(crate::config::schema::AgentsConfig {
            defaults: Some(crate::config::schema::AgentDefaults {
                workspace: Some("/tmp/old-ws".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        new.raw.agents = Some(crate::config::schema::AgentsConfig {
            defaults: Some(crate::config::schema::AgentDefaults {
                workspace: Some("/tmp/new-ws".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        let sections = diff_restart_sections(&old, &new);
        assert!(
            sections.iter().any(|s| s == "config.agents"),
            "expected config.agents in sections, got {sections:?}"
        );
    }

    #[test]
    fn hot_safe_when_only_web_browser_headed_changes() {
        // tools.webBrowser.headed is read live in tools_web.rs via
        // `self.live.ext.read().await.tools.web_browser` — flipping it must
        // not fire the restart banner.
        let mut old = empty_runtime_config();
        let mut new = empty_runtime_config();
        old.raw.tools = Some(crate::config::schema::ToolsConfig {
            loop_detection: None,
            deny: None,
            allow: None,
            exec: None,
            web_search: None,
            web_fetch: None,
            web_browser: Some(crate::config::schema::WebBrowserConfig {
                enabled: None,
                chrome_path: None,
                headed: Some(false),
                profile: None,
                remote_debug_ports: None,
            }),
            computer_use: None,
            upload: None,
            session_result_limits: None,
        });
        new.raw.tools = Some(crate::config::schema::ToolsConfig {
            loop_detection: None,
            deny: None,
            allow: None,
            exec: None,
            web_search: None,
            web_fetch: None,
            web_browser: Some(crate::config::schema::WebBrowserConfig {
                enabled: None,
                chrome_path: None,
                headed: Some(true),
                profile: None,
                remote_debug_ports: None,
            }),
            computer_use: None,
            upload: None,
            session_result_limits: None,
        });
        let sections = diff_restart_sections(&old, &new);
        assert!(
            sections.is_empty(),
            "expected no banner for webBrowser.headed change, got {sections:?}"
        );
        assert!(is_hot_safe_only(&old, &new));
    }

    #[test]
    fn hot_safe_when_only_web_fetch_max_length_changes() {
        // tools.webFetch.maxLength / userAgent / summaryModel are read live
        // in tools_web.rs.
        let mut old = empty_runtime_config();
        let mut new = empty_runtime_config();
        old.raw.tools = Some(crate::config::schema::ToolsConfig {
            loop_detection: None,
            deny: None,
            allow: None,
            exec: None,
            web_search: None,
            web_fetch: Some(crate::config::schema::WebFetchConfig {
                enabled: None,
                max_length: Some(50_000),
                user_agent: None,
                summary_model: None,
            }),
            web_browser: None,
            computer_use: None,
            upload: None,
            session_result_limits: None,
        });
        new.raw.tools = Some(crate::config::schema::ToolsConfig {
            loop_detection: None,
            deny: None,
            allow: None,
            exec: None,
            web_search: None,
            web_fetch: Some(crate::config::schema::WebFetchConfig {
                enabled: None,
                max_length: Some(200_000),
                user_agent: None,
                summary_model: None,
            }),
            web_browser: None,
            computer_use: None,
            upload: None,
            session_result_limits: None,
        });
        assert!(is_hot_safe_only(&old, &new));
    }

    #[test]
    fn hot_safe_when_only_loop_detection_changes() {
        // tools.loopDetection is read live in runtime.rs when constructing
        // the per-turn LoopDetector.
        let mut old = empty_runtime_config();
        let mut new = empty_runtime_config();
        old.raw.tools = Some(crate::config::schema::ToolsConfig {
            loop_detection: Some(crate::config::schema::LoopDetectionConfig {
                enabled: Some(true),
                window: Some(10),
                threshold: Some(20),
                overrides: None,
            }),
            deny: None,
            allow: None,
            exec: None,
            web_search: None,
            web_fetch: None,
            web_browser: None,
            computer_use: None,
            upload: None,
            session_result_limits: None,
        });
        new.raw.tools = Some(crate::config::schema::ToolsConfig {
            loop_detection: Some(crate::config::schema::LoopDetectionConfig {
                enabled: Some(true),
                window: Some(30),
                threshold: Some(40),
                overrides: None,
            }),
            deny: None,
            allow: None,
            exec: None,
            web_search: None,
            web_fetch: None,
            web_browser: None,
            computer_use: None,
            upload: None,
            session_result_limits: None,
        });
        assert!(is_hot_safe_only(&old, &new));
    }

    #[test]
    fn not_hot_safe_when_tools_upload_changes() {
        // tools.upload is read cold by the feishu channel at startup, so it
        // is intentionally NOT whitelisted — flipping it must surface a
        // restart banner until the channel migrates its read site.
        let mut old = empty_runtime_config();
        let mut new = empty_runtime_config();
        old.raw.tools = Some(crate::config::schema::ToolsConfig {
            loop_detection: None,
            deny: None,
            allow: None,
            exec: None,
            web_search: None,
            web_fetch: None,
            web_browser: None,
            computer_use: None,
            upload: Some(crate::config::schema::UploadConfig {
                max_file_size: Some(50 * 1024 * 1024),
                max_text_chars: Some(20_000),
                supports_vision: Some(false),
            }),
            session_result_limits: None,
        });
        new.raw.tools = Some(crate::config::schema::ToolsConfig {
            loop_detection: None,
            deny: None,
            allow: None,
            exec: None,
            web_search: None,
            web_fetch: None,
            web_browser: None,
            computer_use: None,
            upload: Some(crate::config::schema::UploadConfig {
                max_file_size: Some(100 * 1024 * 1024),
                max_text_chars: Some(50_000),
                supports_vision: Some(true),
            }),
            session_result_limits: None,
        });
        let sections = diff_restart_sections(&old, &new);
        assert!(
            sections.iter().any(|s| s == "config.tools"),
            "expected config.tools in sections, got {sections:?}"
        );
    }

    #[test]
    fn not_hot_safe_when_tools_exec_changes() {
        // tools.exec migration was deferred — its read sites still go
        // through `self.config.ext.tools.exec` cold, so changing it must
        // trigger a restart banner.
        use crate::config::schema::ExecToolConfig;

        let mut old = empty_runtime_config();
        let mut new = empty_runtime_config();
        old.raw.tools = Some(crate::config::schema::ToolsConfig {
            loop_detection: None,
            deny: None,
            allow: None,
            exec: Some(ExecToolConfig {
                safety: None,
                timeout_seconds: Some(30),
            }),
            web_search: None,
            web_fetch: None,
            web_browser: None,
            computer_use: None,
            upload: None,
            session_result_limits: None,
        });
        new.raw.tools = Some(crate::config::schema::ToolsConfig {
            loop_detection: None,
            deny: None,
            allow: None,
            exec: Some(ExecToolConfig {
                safety: None,
                timeout_seconds: Some(60),
            }),
            web_search: None,
            web_fetch: None,
            web_browser: None,
            computer_use: None,
            upload: None,
            session_result_limits: None,
        });
        let sections = diff_restart_sections(&old, &new);
        assert!(
            sections.iter().any(|s| s == "config.tools"),
            "expected config.tools in sections, got {sections:?}"
        );
    }
}
