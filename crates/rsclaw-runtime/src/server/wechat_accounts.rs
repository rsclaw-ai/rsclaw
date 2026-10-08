//! Bind a WeChat bot confirmed by a QR login into `channels.wechat.accounts`.
//!
//! The WeChat channel starts one listener per `accounts.<id>.botToken` and
//! only falls back to the top-level `botToken` when no account is
//! configured. A QR login confirmed over HTTP therefore has to land in
//! `accounts`, otherwise a second bot scanned by a user who already has one
//! never comes online. Writing the config file is enough to start it: the
//! config watcher applies `config.channels` changes with a scoped channel
//! reload (see `gateway/startup.rs`).

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

/// Outcome of binding a confirmed bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WechatBinding {
    /// Account id under `channels.wechat.accounts`.
    pub id: String,
    pub label: String,
    /// True when a new account was added; false when an existing account
    /// for the same bot was refreshed (a re-scan renews its token).
    pub created: bool,
}

/// Default label for a bot, matching `rsclaw channels login wechat`.
fn default_label(bot_id: &str) -> String {
    format!("WeChat {}", bot_id.chars().take(8).collect::<String>())
}

fn str_field<'a>(obj: &'a Map<String, Value>, key: &str) -> &'a str {
    obj.get(key).and_then(Value::as_str).unwrap_or("")
}

/// True when an account slot holds no credentials (only a label at most).
fn slot_is_empty(slot: Option<&Value>) -> bool {
    slot.and_then(Value::as_object)
        .is_none_or(|o| o.keys().all(|k| k == "label"))
}

/// Upsert `bot_id` into `root.channels.wechat.accounts`.
///
/// - An account with the same `botId` gets the new token (re-scan = renew).
/// - Legacy layout (top-level `botToken`, no accounts): that bot is what the
///   gateway runs today, so it is moved into `accounts.default` first and
///   keeps working; the new bot is added next to it. A re-scan of the legacy
///   bot itself renews it in place.
/// - Otherwise a new account is added in the same slot `rsclaw channels
///   login wechat` would use: `default` when empty, else `wechat-<hex secs>`.
///
/// Returns the binding and whether `root` changed.
pub(crate) fn upsert_wechat_account(
    root: &mut Value,
    bot_id: &str,
    bot_token: &str,
    now_secs: u64,
) -> Result<(WechatBinding, bool)> {
    let before = root.clone();
    let root_obj = root
        .as_object_mut()
        .context("config root is not an object")?;
    let channels = root_obj
        .entry("channels")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`channels` is not an object")?;
    let wechat = channels
        .entry("wechat")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`channels.wechat` is not an object")?;
    if wechat.get("enabled").and_then(Value::as_bool) != Some(true) {
        wechat.insert("enabled".to_owned(), json!(true));
    }

    let legacy_token = str_field(wechat, "botToken").to_owned();
    let legacy_bot = str_field(wechat, "botId").to_owned();
    let legacy_is_same_bot = !legacy_bot.is_empty() && legacy_bot == bot_id;
    if legacy_is_same_bot {
        // Keep the legacy fields in sync for older readers.
        wechat.insert("botToken".to_owned(), json!(bot_token));
    }

    let accounts = wechat
        .entry("accounts")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`channels.wechat.accounts` is not an object")?;
    let has_active_account = accounts.values().any(|a| {
        a.as_object()
            .is_some_and(|o| !str_field(o, "botToken").is_empty())
    });

    // Legacy single-bot layout: preserve the running bot before adding.
    if !has_active_account
        && !legacy_token.is_empty()
        && !legacy_is_same_bot
        && slot_is_empty(accounts.get("default"))
    {
        let mut slot = Map::new();
        slot.insert("botToken".to_owned(), json!(legacy_token));
        if !legacy_bot.is_empty() {
            slot.insert("botId".to_owned(), json!(legacy_bot));
            slot.insert("label".to_owned(), json!(default_label(&legacy_bot)));
        }
        accounts.insert("default".to_owned(), Value::Object(slot));
    }

    // Same bot already configured: renew its token.
    let existing = accounts.iter().find_map(|(id, a)| {
        a.as_object()
            .filter(|o| str_field(o, "botId") == bot_id)
            .map(|_| id.clone())
    });
    let binding = if let Some(id) = existing {
        let slot = accounts
            .get_mut(&id)
            .and_then(Value::as_object_mut)
            .context("account slot is not an object")?;
        slot.insert("botToken".to_owned(), json!(bot_token));
        let label = match slot.get("label").and_then(Value::as_str) {
            Some(l) if !l.is_empty() => l.to_owned(),
            _ => {
                let l = default_label(bot_id);
                slot.insert("label".to_owned(), json!(l));
                l
            }
        };
        WechatBinding {
            id,
            label,
            created: false,
        }
    } else {
        let id = if slot_is_empty(accounts.get("default")) {
            "default".to_owned()
        } else {
            let base = format!("wechat-{now_secs:x}");
            let mut id = base.clone();
            let mut n = 2;
            while accounts.contains_key(&id) {
                id = format!("{base}-{n}");
                n += 1;
            }
            id
        };
        let label = default_label(bot_id);
        accounts.insert(
            id.clone(),
            json!({"botToken": bot_token, "botId": bot_id, "label": label}),
        );
        // Re-scanning the legacy bot just makes it explicit: not a new bot.
        WechatBinding {
            id,
            label,
            created: !legacy_is_same_bot,
        }
    };

    let changed = *root != before;
    Ok((binding, changed))
}

/// Read the config at `path`, bind the bot and write the file back
/// atomically when something changed. Callers serialize config
/// read-modify-write cycles (see `CONFIG_RMW_LOCK`).
pub(crate) fn bind_wechat_account_at(
    path: &Path,
    bot_id: &str,
    bot_token: &str,
) -> Result<(WechatBinding, bool)> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut val: Value =
        json5::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let (binding, changed) = upsert_wechat_account(&mut val, bot_id, bot_token, now)?;
    if changed {
        rsclaw_config::loader::write_file_atomic(path, &serde_json::to_string_pretty(&val)?)?;
    }
    Ok((binding, changed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounts(root: &Value) -> &Map<String, Value> {
        root["channels"]["wechat"]["accounts"]
            .as_object()
            .expect("accounts")
    }

    #[test]
    fn adds_new_bot_next_to_existing_accounts() {
        let mut root = json!({"channels": {"wechat": {"enabled": true, "accounts": {
            "default": {"botToken": "t-old", "botId": "aaaa1111@im.bot", "label": "Old"}
        }}}});
        let (b, changed) =
            upsert_wechat_account(&mut root, "bbbb2222@im.bot", "t-new", 0xabc).unwrap();
        assert!(changed);
        assert_eq!(
            b,
            WechatBinding {
                id: "wechat-abc".into(),
                label: "WeChat bbbb2222".into(),
                created: true
            }
        );
        let a = accounts(&root);
        assert_eq!(a.len(), 2);
        assert_eq!(a["default"]["botToken"], "t-old");
        assert_eq!(a["wechat-abc"]["botToken"], "t-new");
        assert_eq!(a["wechat-abc"]["botId"], "bbbb2222@im.bot");
    }

    #[test]
    fn rescan_of_same_bot_renews_token_only() {
        let mut root = json!({"channels": {"wechat": {"accounts": {
            "wechat-1": {"botToken": "t-old", "botId": "bbbb2222@im.bot", "label": "Sales"}
        }}}});
        let (b, changed) =
            upsert_wechat_account(&mut root, "bbbb2222@im.bot", "t-renewed", 1).unwrap();
        assert!(changed);
        assert_eq!(
            b,
            WechatBinding {
                id: "wechat-1".into(),
                label: "Sales".into(),
                created: false
            }
        );
        assert_eq!(accounts(&root).len(), 1);
        assert_eq!(accounts(&root)["wechat-1"]["botToken"], "t-renewed");
        // Same token again: nothing to write.
        let (_, changed) =
            upsert_wechat_account(&mut root, "bbbb2222@im.bot", "t-renewed", 2).unwrap();
        assert!(!changed);
    }

    #[test]
    fn legacy_top_level_bot_is_preserved_when_adding() {
        let mut root = json!({"channels": {"wechat": {"enabled": true,
            "botToken": "t-legacy", "botId": "aaaa1111@im.bot"}}});
        let (b, _) = upsert_wechat_account(&mut root, "bbbb2222@im.bot", "t-new", 0x10).unwrap();
        assert!(b.created);
        let a = accounts(&root);
        assert_eq!(a["default"]["botToken"], "t-legacy");
        assert_eq!(a["default"]["botId"], "aaaa1111@im.bot");
        assert_eq!(a[&b.id]["botToken"], "t-new");
        assert_ne!(b.id, "default");
        // Legacy fields stay untouched.
        assert_eq!(root["channels"]["wechat"]["botToken"], "t-legacy");
    }

    #[test]
    fn rescan_of_legacy_bot_becomes_explicit_not_new() {
        let mut root = json!({"channels": {"wechat": {
            "botToken": "t-legacy", "botId": "aaaa1111@im.bot"}}});
        let (b, _) = upsert_wechat_account(&mut root, "aaaa1111@im.bot", "t-renewed", 1).unwrap();
        assert_eq!(b.id, "default");
        assert!(!b.created);
        assert_eq!(accounts(&root).len(), 1);
        assert_eq!(accounts(&root)["default"]["botToken"], "t-renewed");
        assert_eq!(root["channels"]["wechat"]["botToken"], "t-renewed");
    }

    #[test]
    fn first_bot_uses_default_slot() {
        let mut root = json!({"gateway": {"port": 18888}});
        let (b, changed) = upsert_wechat_account(&mut root, "cccc3333@im.bot", "t", 5).unwrap();
        assert!(changed);
        assert_eq!(b.id, "default");
        assert!(b.created);
        assert_eq!(root["channels"]["wechat"]["enabled"], true);
    }

    #[test]
    fn colliding_slot_ids_get_a_suffix() {
        let mut root = json!({"channels": {"wechat": {"accounts": {
            "default": {"botToken": "t1", "botId": "a@im.bot"},
            "wechat-5": {"botToken": "t2", "botId": "b@im.bot"}
        }}}});
        let (b, _) = upsert_wechat_account(&mut root, "c@im.bot", "t3", 5).unwrap();
        assert_eq!(b.id, "wechat-5-2");
    }

    #[test]
    fn file_write_is_atomic_and_idempotent() {
        let dir = std::env::temp_dir().join(format!("rsclaw-wxbind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rsclaw.json5");
        std::fs::write(&path, "// comment\n{ gateway: { port: 19001 }, }\n").unwrap();
        let (b, changed) = bind_wechat_account_at(&path, "dddd4444@im.bot", "tok").unwrap();
        assert!(changed && b.created);
        let parsed: Value = json5::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["gateway"]["port"], 19001);
        assert_eq!(
            parsed["channels"]["wechat"]["accounts"]["default"]["botToken"],
            "tok"
        );
        // No temp files left behind next to the config.
        let stray = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp")
            })
            .count();
        assert_eq!(stray, 0);
        let (b2, changed2) = bind_wechat_account_at(&path, "dddd4444@im.bot", "tok").unwrap();
        assert!(!changed2 && !b2.created);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn concurrent_confirmations_write_once() {
        let dir = std::env::temp_dir().join(format!("rsclaw-wxbind-c-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rsclaw.json5");
        std::fs::write(&path, "{}").unwrap();
        let run = |p: std::path::PathBuf| async move {
            let _g = super::super::CONFIG_RMW_LOCK.lock().await;
            bind_wechat_account_at(&p, "eeee5555@im.bot", "tok").unwrap()
        };
        let (a, b) = tokio::join!(run(path.clone()), run(path.clone()));
        let writes = [a.1, b.1].iter().filter(|c| **c).count();
        let created = [a.0.created, b.0.created].iter().filter(|c| **c).count();
        assert_eq!(writes, 1);
        assert_eq!(created, 1);
        let parsed: Value = json5::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            parsed["channels"]["wechat"]["accounts"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
