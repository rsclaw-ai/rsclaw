//! Populate the sender-trust registry (`rsclaw_agent::trust`) from config.
//!
//! Walks `channels.*` generically (top-level `allowFrom` plus
//! `accounts.<name>.allowFrom`, and `custom[].allowFrom`) so every channel,
//! including account-scoped ones stored as raw JSON, contributes its static
//! allowlist. Called at gateway start and after every config reload.

use std::collections::{HashMap, HashSet};

use rsclaw_config::runtime::RuntimeConfig;
use serde_json::Value;

/// Rebuild explicit owners and static per-channel allowlists from `config`.
pub fn refresh_from_config(config: &RuntimeConfig) {
    let owners = config
        .raw
        .gateway
        .as_ref()
        .and_then(|g| g.owners.clone())
        .unwrap_or_default();
    rsclaw_agent::trust::set_explicit_owners(owners);

    rsclaw_util::net::set_trusted_redirect_sites(
        config
            .raw
            .gateway
            .as_ref()
            .and_then(|g| g.trusted_redirect_sites.clone()),
    );

    let mut lists: HashMap<String, HashSet<String>> = HashMap::new();
    if let Some(channels) = config.raw.channels.as_ref() {
        match serde_json::to_value(channels) {
            Ok(Value::Object(map)) => {
                for (name, section) in map {
                    if name == "custom" {
                        if let Value::Array(items) = &section {
                            for item in items {
                                if let Some(cname) = item.get("name").and_then(Value::as_str) {
                                    collect_allow_from(item, lists.entry(cname.to_owned()).or_default());
                                }
                            }
                        }
                        continue;
                    }
                    let set = lists.entry(name).or_default();
                    collect_allow_from(&section, set);
                    if let Some(Value::Object(accounts)) = section.get("accounts") {
                        for acct in accounts.values() {
                            collect_allow_from(acct, set);
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("trust: failed to serialize channels config: {e}"),
        }
    }
    let static_count: usize = lists.values().map(|s| s.iter().filter(|p| *p != "*").count()).sum();
    let explicit_count = config
        .raw
        .gateway
        .as_ref()
        .and_then(|g| g.owners.as_ref())
        .map_or(0, Vec::len);
    if static_count == 0 && explicit_count == 0 && config.raw.channels.is_some() {
        tracing::info!(
            "no channel owners configured: messaging-channel senders cannot use owner-only \
             tools or host commands. Add `gateway.owners: [\"<channel>:<peer_id>\"]` or list \
             your id in the channel's `allowFrom` to grant yourself owner access."
        );
    }
    rsclaw_agent::trust::set_static_allowlists(lists);
}

fn collect_allow_from(section: &Value, out: &mut HashSet<String>) {
    for key in ["allowFrom", "allow_from"] {
        if let Some(Value::Array(ids)) = section.get(key) {
            for id in ids {
                match id {
                    Value::String(s) => {
                        out.insert(s.trim().to_owned());
                    }
                    Value::Number(n) => {
                        out.insert(n.to_string());
                    }
                    _ => {}
                }
            }
        }
    }
}
