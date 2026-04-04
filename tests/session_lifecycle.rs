//! Integration tests for the session lifecycle at the store layer.
//!
//! These tests exercise RedbStore directly — no HTTP server is required.

use rsclaw::{
    MemoryTier,
    store::redb_store::{RedbStore, SessionMeta},
};

fn open_store() -> (RedbStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = RedbStore::open(&dir.path().join("test.redb"), MemoryTier::Low).expect("open redb");
    (store, dir)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn empty_store_list_sessions_returns_empty() {
    let (store, _dir) = open_store();
    let sessions = store.list_sessions().expect("list_sessions");
    assert!(
        sessions.is_empty(),
        "expected empty list from a fresh store, got: {sessions:?}"
    );
}

#[test]
fn put_and_get_session_meta_roundtrip() {
    let (store, _dir) = open_store();

    let key = "agent:main:telegram:direct:u42";
    let meta = SessionMeta {
        session_key: key.to_owned(),
        message_count: 7,
        last_active: 1_700_000_100,
        created_at: 1_700_000_000,
    };

    store
        .put_session_meta(key, &meta)
        .expect("put_session_meta");

    let got = store.get_session_meta(key).expect("get_session_meta");
    assert!(got.is_some(), "expected Some after put, got None");

    let got = got.unwrap();
    assert_eq!(got.session_key, key);
    assert_eq!(got.message_count, 7);
    assert_eq!(got.last_active, 1_700_000_100);
    assert_eq!(got.created_at, 1_700_000_000);
}

#[test]
fn put_session_appears_in_list() {
    let (store, _dir) = open_store();

    let keys = ["sess:alpha", "sess:beta", "sess:gamma"];
    for k in &keys {
        let meta = SessionMeta {
            session_key: k.to_string(),
            message_count: 0,
            last_active: 0,
            created_at: 0,
        };
        store.put_session_meta(k, &meta).expect("put");
    }

    let listed = store.list_sessions().expect("list_sessions");
    for k in &keys {
        assert!(
            listed.contains(&k.to_string()),
            "expected {k} in list, got: {listed:?}"
        );
    }
}

#[test]
fn delete_session_removes_it() {
    let (store, _dir) = open_store();

    let key = "agent:main:cli:direct:del_user";
    store
        .append_message(
            key,
            &serde_json::json!({"role": "user", "content": "hello"}),
        )
        .expect("append_message");

    // Verify it exists first.
    assert!(
        store.get_session_meta(key).expect("get").is_some(),
        "session should exist before delete"
    );

    store.delete_session(key).expect("delete_session");

    // Meta must be gone.
    assert!(
        store
            .get_session_meta(key)
            .expect("get after delete")
            .is_none(),
        "session meta should be None after delete"
    );

    // Messages must be gone too.
    let msgs = store
        .load_messages(key)
        .expect("load_messages after delete");
    assert!(
        msgs.is_empty(),
        "messages should be empty after delete, got: {msgs:?}"
    );

    // Must not appear in list.
    let listed = store.list_sessions().expect("list_sessions after delete");
    assert!(
        !listed.contains(&key.to_string()),
        "key should not be in list after delete, got: {listed:?}"
    );
}
