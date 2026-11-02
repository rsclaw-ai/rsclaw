//! Process-wide cache of sender display names seen on inbound channel events.
//!
//! Channel parsers record the human-readable name an inbound event already
//! carries (Telegram `from.first_name`, Discord `author.global_name`,
//! DingTalk `senderNick`, ...) keyed by `(channel, peer_id)`. The agent
//! runtime looks it up when it persists the turn and stores it on the
//! session metadata, so session lists can show a person instead of an id.
//! No platform API is called to resolve names.

use std::{collections::HashMap, sync::Mutex};

/// Max cached entries; the cache is cleared wholesale when exceeded (names
/// are re-recorded on the next inbound message, so loss is harmless).
const PEER_NAME_CAP: usize = 10_000;

/// Max chars kept for a display name.
const PEER_NAME_MAX_CHARS: usize = 64;

static PEER_NAMES: std::sync::LazyLock<Mutex<HashMap<String, String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache_key(channel: &str, peer_id: &str) -> String {
    format!("{channel}\0{peer_id}")
}

/// Record the display name of `peer_id` on `channel`. Blank names and ids
/// are ignored.
pub fn record_peer_name(channel: &str, peer_id: &str, name: &str) {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if peer_id.is_empty() || name.is_empty() {
        return;
    }
    let name = rsclaw_util::session_text::truncate_chars(&name, PEER_NAME_MAX_CHARS).to_owned();
    match PEER_NAMES.lock() {
        Ok(mut map) => {
            if map.len() >= PEER_NAME_CAP {
                map.clear();
            }
            map.insert(cache_key(channel, peer_id), name);
        }
        Err(e) => tracing::warn!("peer name cache lock poisoned: {e}"),
    }
}

/// Last display name recorded for `peer_id` on `channel`, if any.
pub fn peer_name(channel: &str, peer_id: &str) -> Option<String> {
    match PEER_NAMES.lock() {
        Ok(map) => map.get(&cache_key(channel, peer_id)).cloned(),
        Err(e) => {
            tracing::warn!("peer name cache lock poisoned: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_name_record_and_lookup() {
        record_peer_name("test-ch", "u1", "  Alice   Smith ");
        assert_eq!(peer_name("test-ch", "u1").as_deref(), Some("Alice Smith"));
        record_peer_name("test-ch", "u2", "   ");
        assert!(peer_name("test-ch", "u2").is_none());
        assert!(peer_name("other-ch", "u1").is_none());
    }
}
