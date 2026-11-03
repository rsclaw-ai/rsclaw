//! Per-conversation headers on fleet requests.
//!
//! An engine extension can attach extra request headers to every request
//! this engine sends to the rsclaw fleet on behalf of one conversation
//! (session key) with [`set_header`]; for example a billing extension tags
//! the model calls of a court filing. The agent runtime runs each turn inside
//! [`scope`], so [`FleetHttp`](crate::FleetHttp) knows whose request it is
//! sending and adds that conversation's headers. Work that leaves the turn's
//! task (`tokio::spawn`, `spawn_blocking`) takes [`current`] along and
//! re-enters it with [`within`] / [`sync_within`].
//!
//! A sub-conversation (`<parent>:task:<id>`, `<parent>:send:<id>`,
//! `<parent>:a2a:<agent>`) uses its parent's headers unless it has its own.
//!
//! Only `x-` header names are accepted, so nothing here can replace the
//! authorization or content headers.

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    sync::{Arc, LazyLock, RwLock},
};

tokio::task_local! {
    static SESSION: Arc<str>;
}

type Headers = BTreeMap<String, String>;

static HEADERS: LazyLock<RwLock<HashMap<String, Headers>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Markers that join a sub-conversation's key to its parent's.
const CHILD_MARKERS: [&str; 3] = [":task:", ":send:", ":a2a:"];

/// Longest header value accepted.
const MAX_VALUE_LEN: usize = 256;

fn valid_name(name: &str) -> bool {
    name.len() > 2
        && name.len() <= 64
        && name.starts_with("x-")
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_VALUE_LEN
        && value.bytes().all(|b| (0x21..0x7f).contains(&b))
}

/// Set (`Some`) or remove (`None`) header `name` on the fleet requests of
/// conversation `session_key`. `name` is lower-cased; it must be an `x-`
/// name of letters, digits and `-`, and the value visible ASCII without
/// spaces (at most 256 bytes). Returns false (and changes nothing) for
/// anything else.
pub fn set_header(session_key: &str, name: &str, value: Option<&str>) -> bool {
    let name = name.to_ascii_lowercase();
    if session_key.is_empty() || !valid_name(&name) || value.is_some_and(|v| !valid_value(v)) {
        return false;
    }
    let mut all = HEADERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match value {
        Some(value) => {
            all.entry(session_key.to_owned())
                .or_default()
                .insert(name, value.to_owned());
        }
        None => {
            if let Some(headers) = all.get_mut(session_key) {
                headers.remove(&name);
                if headers.is_empty() {
                    all.remove(session_key);
                }
            }
        }
    }
    true
}

/// Remove header `name` wherever it has `value` (whichever conversation it
/// was set on). Returns how many conversations had it.
pub fn clear_value(name: &str, value: &str) -> usize {
    let name = name.to_ascii_lowercase();
    let mut all = HEADERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut cleared = 0;
    all.retain(|_, headers| {
        if headers.get(&name).is_some_and(|v| v == value) {
            headers.remove(&name);
            cleared += 1;
        }
        !headers.is_empty()
    });
    cleared
}

/// The value of header `name` set on `session_key` itself (no parent).
pub fn header(session_key: &str, name: &str) -> Option<String> {
    HEADERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_key)?
        .get(&name.to_ascii_lowercase())
        .cloned()
}

/// The parent of a sub-conversation key, if it is one.
fn parent_of(session_key: &str) -> Option<&str> {
    CHILD_MARKERS
        .iter()
        .filter_map(|marker| session_key.rfind(marker))
        .max()
        .map(|at| &session_key[..at])
        .filter(|parent| !parent.is_empty())
}

/// The headers of `session_key`'s fleet requests: its own, else those of
/// the nearest parent conversation that has any.
pub fn headers_of(session_key: &str) -> Vec<(String, String)> {
    let all = HEADERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if all.is_empty() {
        return Vec::new();
    }
    let mut key = Some(session_key);
    while let Some(k) = key {
        if let Some(headers) = all.get(k) {
            return headers
                .iter()
                .map(|(n, v)| (n.clone(), v.clone()))
                .collect();
        }
        key = parent_of(k);
    }
    Vec::new()
}

/// The conversation the current task works for, if any.
pub fn current() -> Option<Arc<str>> {
    SESSION.try_with(Arc::clone).ok()
}

/// The headers for a fleet request made now (none outside a conversation).
pub fn current_headers() -> Vec<(String, String)> {
    current().map_or_else(Vec::new, |s| headers_of(&s))
}

/// Run `f` as work of conversation `session_key`.
pub async fn scope<F: Future>(session_key: Arc<str>, f: F) -> F::Output {
    SESSION.scope(session_key, f).await
}

/// [`scope`] when there is a conversation, else just `f`.
pub async fn within<F: Future>(session: Option<Arc<str>>, f: F) -> F::Output {
    match session {
        Some(s) => SESSION.scope(s, f).await,
        None => f.await,
    }
}

/// Synchronous [`within`], for `spawn_blocking` closures (futures they
/// block on run in it too).
pub fn sync_within<R>(session: Option<Arc<str>>, f: impl FnOnce() -> R) -> R {
    match session {
        Some(s) => SESSION.sync_scope(s, f),
        None => f(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_values_are_checked() {
        assert!(!set_header("fs-check", "authorization", Some("x")));
        assert!(!set_header("fs-check", "X Bad", Some("x")));
        assert!(!set_header("fs-check", "x-ok", Some("has space")));
        assert!(!set_header("", "x-ok", Some("v")));
        assert!(set_header("fs-check", "X-Rsclaw-Test", Some("v1")));
        assert_eq!(header("fs-check", "x-rsclaw-test").as_deref(), Some("v1"));
        assert!(set_header("fs-check", "x-rsclaw-test", None));
        assert_eq!(header("fs-check", "x-rsclaw-test"), None);
    }

    #[test]
    fn children_use_the_parents_headers() {
        let parent = "agent:main:desktop:fs-parent";
        set_header(parent, "x-rsclaw-filing", Some("f-1"));
        let want = vec![("x-rsclaw-filing".to_owned(), "f-1".to_owned())];
        assert_eq!(headers_of(parent), want);
        assert_eq!(headers_of(&format!("{parent}:task:ab12")), want);
        assert_eq!(headers_of(&format!("{parent}:task:ab12:send:9")), want);
        assert!(headers_of("agent:main:desktop:fs-other").is_empty());
        // A child with its own headers uses its own.
        let child = format!("{parent}:a2a:helper");
        set_header(&child, "x-rsclaw-filing", Some("f-2"));
        assert_eq!(headers_of(&child)[0].1, "f-2");
        set_header(parent, "x-rsclaw-filing", None);
        assert_eq!(clear_value("x-rsclaw-filing", "f-2"), 1);
        assert!(headers_of(parent).is_empty());
        assert!(headers_of(&child).is_empty());
    }

    #[tokio::test]
    async fn the_scope_follows_the_task_and_blocking_work() {
        let key = "agent:main:desktop:fs-scope";
        set_header(key, "x-rsclaw-filing", Some("f-3"));
        assert!(current_headers().is_empty());
        let seen = scope(Arc::from(key), async {
            let session = current();
            let blocking = tokio::task::spawn_blocking({
                let session = session.clone();
                move || sync_within(session, current_headers)
            })
            .await
            .unwrap();
            let spawned = tokio::spawn(within(session, async { current_headers() }))
                .await
                .unwrap();
            (current_headers(), blocking, spawned)
        })
        .await;
        let want = vec![("x-rsclaw-filing".to_owned(), "f-3".to_owned())];
        assert_eq!(seen, (want.clone(), want.clone(), want));
        set_header(key, "x-rsclaw-filing", None);
    }
}
