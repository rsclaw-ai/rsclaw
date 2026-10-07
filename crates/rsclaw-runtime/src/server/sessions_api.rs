//! Session list detail / rename / pin / archive / search, shared by the HTTP
//! API (`/api/v1/sessions*`) and the WebSocket `sessions.*` methods.
//!
//! Everything here is synchronous redb work over a [`RedbStore`]; HTTP
//! handlers run it on the blocking pool.

use rsclaw_store::redb_store::{RedbStore, SessionMeta, SessionPatch, TitleSource};
use rsclaw_util::session_text::{
    USER_TITLE_MAX_CHARS, match_snippet, message_text, strip_injected_context, truncate_chars,
};
use serde_json::{Value, json};

use crate::gateway::session::parse_session_key;

/// Max recent messages scanned per session by search.
const SEARCH_SCAN_MESSAGES: usize = 300;

/// Snippet width (chars) around a search hit.
const SNIPPET_CHARS: usize = 60;

/// Default / max number of search results.
pub(crate) const SEARCH_DEFAULT_LIMIT: usize = 50;
pub(crate) const SEARCH_MAX_LIMIT: usize = 200;

/// One session as shown in a session list.
#[derive(Debug, Clone)]
pub(crate) struct SessionDetail {
    pub key: String,
    pub title: Option<String>,
    pub title_source: Option<TitleSource>,
    pub agent_id: Option<String>,
    pub channel: Option<String>,
    pub peer_name: Option<String>,
    pub pinned: bool,
    pub archived: bool,
    pub created_at: i64,
    pub last_active: i64,
    pub message_count: u64,
}

fn title_source_str(src: Option<TitleSource>) -> Option<&'static str> {
    src.map(|s| match s {
        TitleSource::Auto => "auto",
        TitleSource::User => "user",
    })
}

fn rfc3339(ts: i64) -> Value {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| Value::String(dt.to_rfc3339()))
        .unwrap_or(Value::Null)
}

impl SessionDetail {
    /// HTTP shape (snake_case, Unix-second timestamps).
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "key": self.key,
            "title": self.title,
            "title_source": title_source_str(self.title_source),
            "agent_id": self.agent_id,
            "channel": self.channel,
            "peer_name": self.peer_name,
            "pinned": self.pinned,
            "archived": self.archived,
            "created_at": self.created_at,
            "last_active": self.last_active,
            "message_count": self.message_count,
        })
    }

    /// WebSocket shape (camelCase, RFC 3339 timestamps like `sessions.list`).
    pub(crate) fn to_ws_json(&self) -> Value {
        json!({
            "key": self.key,
            "sessionKey": self.key,
            "title": self.title,
            "titleSource": title_source_str(self.title_source),
            "agentId": self.agent_id,
            "channel": self.channel,
            "peerName": self.peer_name,
            "pinned": self.pinned,
            "archived": self.archived,
            "createdAt": rfc3339(self.created_at),
            "updatedAt": rfc3339(self.last_active),
            "messageCount": self.message_count,
        })
    }
}

/// Build the detail view of one session from its (already titled) meta.
fn detail_from_meta(db: &RedbStore, key: &str, meta: SessionMeta) -> SessionDetail {
    let (agent_id, channel) = parse_session_key(key);
    let message_count = db.count_active_messages(key).unwrap_or_else(|e| {
        tracing::warn!(session = %key, error = %e, "active message count failed");
        0
    });
    SessionDetail {
        key: key.to_owned(),
        title: meta.title,
        title_source: meta.title_source,
        agent_id,
        channel,
        peer_name: meta.peer_name,
        pinned: meta.pinned,
        archived: meta.archived,
        created_at: meta.created_at,
        last_active: meta.last_active,
        message_count,
    }
}

/// Detail view of `key`, lazily backfilling its title. `None` when the
/// session does not exist.
pub(crate) fn session_detail(db: &RedbStore, key: &str) -> anyhow::Result<Option<SessionDetail>> {
    if let Err(e) = db.ensure_session_title(key) {
        tracing::warn!(session = %key, error = %e, "session title backfill failed");
    }
    Ok(db
        .get_session_meta(key)?
        .map(|meta| detail_from_meta(db, key, meta)))
}

/// All sessions in list order: pinned first, then by `last_active` desc.
/// Archived sessions are skipped unless `include_archived`.
pub(crate) fn list_details(db: &RedbStore, include_archived: bool) -> anyhow::Result<Vec<SessionDetail>> {
    let mut out = Vec::new();
    for key in db.list_sessions()? {
        match session_detail(db, &key) {
            Ok(Some(d)) if include_archived || !d.archived => out.push(d),
            Ok(_) => {}
            Err(e) => tracing::warn!(session = %key, error = %e, "skip unreadable session"),
        }
    }
    out.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(b.last_active.cmp(&a.last_active))
    });
    Ok(out)
}

/// Failure of a session patch, mapped to HTTP 400 / 404 / 500 or the
/// matching WS error codes.
#[derive(Debug)]
pub(crate) enum PatchError {
    BadRequest(String),
    NotFound,
    Internal(String),
}

/// Parse a patch body `{"title"?: string|null, "pinned"?: bool, "archived"?: bool}`.
/// An empty / whitespace-only title or `null` clears the user title; a
/// non-empty title is trimmed and capped at [`USER_TITLE_MAX_CHARS`] chars.
pub(crate) fn parse_patch(body: &Value) -> Result<SessionPatch, String> {
    let obj = body
        .as_object()
        .ok_or_else(|| "body must be a JSON object".to_owned())?;
    let mut patch = SessionPatch::default();
    if let Some(t) = obj.get("title") {
        patch.title = Some(match t {
            Value::Null => None,
            Value::String(s) => {
                let s = s.trim();
                (!s.is_empty()).then(|| truncate_chars(s, USER_TITLE_MAX_CHARS).trim_end().to_owned())
            }
            _ => return Err("title must be a string or null".to_owned()),
        });
    }
    for (field, slot) in [("pinned", &mut patch.pinned), ("archived", &mut patch.archived)] {
        if let Some(v) = obj.get(field) {
            *slot = Some(v.as_bool().ok_or_else(|| format!("{field} must be a boolean"))?);
        }
    }
    if patch.title.is_none() && patch.pinned.is_none() && patch.archived.is_none() {
        return Err("nothing to update: expected title, pinned or archived".to_owned());
    }
    Ok(patch)
}

/// Apply a patch body to session `key` and return its updated detail view.
pub(crate) fn apply_patch(db: &RedbStore, key: &str, body: &Value) -> Result<SessionDetail, PatchError> {
    let patch = parse_patch(body).map_err(PatchError::BadRequest)?;
    match db.patch_session(key, &patch) {
        Ok(Some(_)) => {}
        Ok(None) => return Err(PatchError::NotFound),
        Err(e) => return Err(PatchError::Internal(e.to_string())),
    }
    match session_detail(db, key) {
        Ok(Some(d)) => Ok(d),
        Ok(None) => Err(PatchError::NotFound),
        Err(e) => Err(PatchError::Internal(e.to_string())),
    }
}

/// Add `display_text` (original text without the runtime-injected context
/// prefix) and `sent_at` (`"YYYY-MM-DD HH:MM"` or null) to a user message.
/// Other roles are left unchanged.
pub(crate) fn decorate_message(mut msg: Value) -> Value {
    if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
        return msg;
    }
    let text = message_text(&msg);
    let (sent_at, body) = strip_injected_context(&text);
    let body = body.to_owned();
    if let Some(obj) = msg.as_object_mut() {
        obj.insert("display_text".to_owned(), Value::String(body));
        obj.insert("sent_at".to_owned(), sent_at.map(Value::String).unwrap_or(Value::Null));
    }
    msg
}

/// Search session titles and recent message content (case-insensitive
/// substring). Title hits rank first, then most recently active. Each
/// result is a detail object plus `snippet` and `match` (`"title"` or
/// `"content"`). Archived sessions are included (flagged by `archived`).
pub(crate) fn search(db: &RedbStore, query: &str, limit: usize) -> anyhow::Result<Vec<Value>> {
    let query = query.trim();
    let mut hits: Vec<(bool, SessionDetail, String)> = Vec::new();
    for key in db.list_sessions()? {
        let detail = match session_detail(db, &key) {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(session = %key, error = %e, "search: skip unreadable session");
                continue;
            }
        };
        if let Some(snippet) = detail
            .title
            .as_deref()
            .and_then(|t| match_snippet(t, query, SNIPPET_CHARS))
        {
            hits.push((true, detail, snippet));
            continue;
        }
        let messages = match db.load_recent_messages(&key, SEARCH_SCAN_MESSAGES) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(session = %key, error = %e, "search: message scan failed");
                continue;
            }
        };
        let snippet = messages.iter().rev().find_map(|m| {
            let role = m.get("role").and_then(|r| r.as_str())?;
            if role != "user" && role != "assistant" {
                return None;
            }
            let text = message_text(m);
            if text.starts_with("[CONTEXT COMPACTION") {
                return None;
            }
            let body = if role == "user" { strip_injected_context(&text).1 } else { text.as_str() };
            match_snippet(body, query, SNIPPET_CHARS)
        });
        if let Some(snippet) = snippet {
            hits.push((false, detail, snippet));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.last_active.cmp(&a.1.last_active)));
    Ok(hits
        .into_iter()
        .take(limit)
        .map(|(is_title, detail, snippet)| {
            let mut v = detail.to_json();
            v["snippet"] = Value::String(snippet);
            v["match"] = Value::String(if is_title { "title" } else { "content" }.to_owned());
            v
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use rsclaw_platform::MemoryTier;

    use super::*;

    fn open_db() -> (RedbStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = RedbStore::open(&dir.path().join("t.redb"), MemoryTier::Low).expect("open");
        (db, dir)
    }

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": format!(
            "Now: 2026-10-06 07:23 Mon CST\n[Session started: 2026-10-06 07:23 Monday, CST, via feishu]\n{text}"
        )})
    }

    #[test]
    fn sessions_api_patch_list_search() {
        let (db, _dir) = open_db();
        let a = "agent:main:feishu:direct:ou_a";
        let b = "desktop:main:b1";
        db.append_message(a, &user("明天北京天气怎么样")).expect("append");
        db.append_message(a, &json!({"role": "assistant", "content": "明天晴，最高 25 度"}))
            .expect("append");
        db.append_message(b, &json!({"role": "user", "content": "Rust lifetimes question"}))
            .expect("append");

        // Detail list backfills titles and parses the key.
        let list = list_details(&db, false).expect("list");
        assert_eq!(list.len(), 2);
        let da = list.iter().find(|d| d.key == a).expect("a");
        assert_eq!(da.title.as_deref(), Some("明天北京天气怎么样"));
        assert_eq!(da.agent_id.as_deref(), Some("main"));
        assert_eq!(da.channel.as_deref(), Some("feishu"));
        assert_eq!(da.message_count, 2);
        let ja = da.to_json();
        assert_eq!(ja["title_source"], "auto");

        // Rename + pin; pinned sorts first.
        let d = apply_patch(&db, b, &json!({"title": "  Rust 生命周期  ", "pinned": true}))
            .expect("patch");
        assert_eq!(d.title.as_deref(), Some("Rust 生命周期"));
        assert_eq!(d.title_source, Some(TitleSource::User));
        let list = list_details(&db, false).expect("list");
        assert_eq!(list[0].key, b);

        // Empty title clears back to an auto title.
        let d = apply_patch(&db, b, &json!({"title": ""})).expect("patch");
        assert_eq!(d.title.as_deref(), Some("Rust lifetimes question"));
        assert_eq!(d.title_source, Some(TitleSource::Auto));

        // Archive hides from the default list.
        apply_patch(&db, a, &json!({"archived": true})).expect("patch");
        assert_eq!(list_details(&db, false).expect("list").len(), 1);
        assert_eq!(list_details(&db, true).expect("list").len(), 2);

        // Errors.
        assert!(matches!(apply_patch(&db, "nope", &json!({"pinned": true})), Err(PatchError::NotFound)));
        assert!(matches!(apply_patch(&db, a, &json!({})), Err(PatchError::BadRequest(_))));
        assert!(matches!(apply_patch(&db, a, &json!({"pinned": "yes"})), Err(PatchError::BadRequest(_))));

        // Search: title hit ranks before content hit.
        let res = search(&db, "rust", 10).expect("search");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0]["match"], "title");
        let res = search(&db, "25 度", 10).expect("search");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0]["key"], a);
        assert_eq!(res[0]["match"], "content");
        assert!(res[0]["snippet"].as_str().expect("snippet").contains("25 度"));
        // The injected context prefix is not searchable content.
        assert!(search(&db, "Session started", 10).expect("search").is_empty());
    }

    #[test]
    fn sessions_api_decorate_message() {
        let m = decorate_message(user("你好"));
        assert_eq!(m["display_text"], "你好");
        assert_eq!(m["sent_at"], "2026-10-06 07:23");
        let m = decorate_message(json!({"role": "user", "content": "plain"}));
        assert_eq!(m["display_text"], "plain");
        assert!(m["sent_at"].is_null());
        let m = decorate_message(json!({"role": "assistant", "content": "x"}));
        assert!(m.get("display_text").is_none());
    }
}
