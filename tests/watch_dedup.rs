//! Dedup + concurrency-limit behavior for WatchRegistry.

use std::sync::Arc;

use rsclaw::gateway::watch::{Origin, WatchCommandReply, WatchRegistry};

fn fresh_registry() -> Arc<WatchRegistry> {
    WatchRegistry::init_for_test()
}

#[tokio::test]
async fn manual_reissue_restarts_single_watch() {
    // Since 6dc53377 (crate-split) a MANUAL re-issue of the same source is
    // the one-command recovery for a wedged zombie: stop the old watch and
    // spawn a fresh one. Cron re-issues stay silent (see the cron test).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dedup.log");
    tokio::fs::write(&path, "").await.unwrap();
    let src = format!("{}", path.display());

    let reg = fresh_registry();
    let r1 = reg
        .clone()
        .handle_command("cli", "user1", None, &src, Origin::User)
        .await;
    let r2 = reg
        .clone()
        .handle_command("cli", "user1", None, &src, Origin::User)
        .await;

    let (id1, id2) = match (&r1, &r2) {
        (WatchCommandReply::Reply(a), WatchCommandReply::Reply(b)) => {
            assert!(a.starts_with("Watch started: w_"), "first call: {a}");
            assert!(b.starts_with("Watch started: w_"), "second call: {b}");
            (extract_id(a), extract_id(b))
        }
        _ => panic!(
            "unexpected reply shapes: {:?} / {:?}",
            classify(&r1),
            classify(&r2)
        ),
    };
    assert_ne!(id1, id2, "re-issue must restart with a fresh id");

    // Restart replaced the old entry — the registry holds exactly one watch.
    let list = reg
        .clone()
        .handle_command("cli", "user1", None, "list", Origin::User)
        .await;
    match list {
        WatchCommandReply::Reply(s) => {
            assert!(s.contains("Active watches (1/5)"), "got: {s}");
        }
        _ => panic!("expected list reply"),
    }

    // Cleanup.
    let _ = reg.stop_all_for("cli", "user1").await;
}

#[tokio::test]
async fn dedup_returns_silent_for_cron_origin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dedup-cron.log");
    tokio::fs::write(&path, "").await.unwrap();
    let src = format!("{}", path.display());

    let reg = fresh_registry();
    let r1 = reg
        .clone()
        .handle_command("cli", "user2", None, &src, Origin::Cron)
        .await;
    let r2 = reg
        .clone()
        .handle_command("cli", "user2", None, &src, Origin::Cron)
        .await;

    assert!(
        matches!(r1, WatchCommandReply::Reply(_)),
        "first call should reply (re)started"
    );
    assert!(
        matches!(r2, WatchCommandReply::Silent),
        "cron dedup hit should be silent"
    );

    let _ = reg.stop_all_for("cli", "user2").await;
}

#[tokio::test]
async fn dedup_keys_on_normalized_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("normalize.log");
    tokio::fs::write(&path, "").await.unwrap();
    let p = path.display().to_string();
    let src_a = format!("file {}", p);
    let src_b = format!("file  {}", p); // extra whitespace

    let reg = fresh_registry();
    let r1 = reg
        .clone()
        .handle_command("cli", "user3", None, &src_a, Origin::User)
        .await;
    let r2 = reg
        .clone()
        .handle_command("cli", "user3", None, &src_b, Origin::User)
        .await;

    let started_id = match r1 {
        WatchCommandReply::Reply(s) => extract_id(&s),
        _ => panic!("expected Reply"),
    };
    assert!(started_id.starts_with('w'));
    let reissued_id = match r2 {
        WatchCommandReply::Reply(s) => extract_id(&s),
        _ => panic!("expected Reply"),
    };
    assert_ne!(
        reissued_id, started_id,
        "normalized-equal source should hit the same dedup key (restart path)"
    );

    // src_b normalizes to the same key as src_a, so the re-issue replaced
    // the old watch instead of double-spawning: exactly one active watch.
    let list = reg
        .clone()
        .handle_command("cli", "user3", None, "list", Origin::User)
        .await;
    match list {
        WatchCommandReply::Reply(s) => {
            assert!(s.contains("Active watches (1/5)"), "got: {s}");
        }
        _ => panic!("expected list reply"),
    }

    let _ = reg.stop_all_for("cli", "user3").await;
}

#[tokio::test]
async fn dedup_distinguishes_channel_and_peer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi.log");
    tokio::fs::write(&path, "").await.unwrap();
    let src = format!("{}", path.display());

    let reg = fresh_registry();
    // Same source under three different (channel, peer) pairs — each gets its own
    // watch.
    let r1 = reg
        .clone()
        .handle_command("cli", "alice", None, &src, Origin::User)
        .await;
    let r2 = reg
        .clone()
        .handle_command("cli", "bob", None, &src, Origin::User)
        .await;
    let r3 = reg
        .clone()
        .handle_command("feishu", "alice", None, &src, Origin::User)
        .await;
    for (i, r) in [r1, r2, r3].iter().enumerate() {
        if let WatchCommandReply::Reply(s) = r {
            assert!(s.starts_with("Watch started: w_"), "#{i}: {s}");
        } else {
            panic!("#{i}: expected Reply");
        }
    }

    let _ = reg.stop_all_for("cli", "alice").await;
    let _ = reg.stop_all_for("cli", "bob").await;
    let _ = reg.stop_all_for("feishu", "alice").await;
}

#[tokio::test]
async fn limit_enforced_at_six_concurrent_watches() {
    let dir = tempfile::tempdir().unwrap();
    // Five distinct files under one peer = 5 watches, sixth must be rejected.
    let mut paths = Vec::new();
    for i in 0..6 {
        let p = dir.path().join(format!("limit-{i}.log"));
        tokio::fs::write(&p, "").await.unwrap();
        paths.push(p);
    }

    let reg = fresh_registry();
    for (i, p) in paths.iter().enumerate().take(5) {
        let src = format!("{}", p.display());
        let r = reg
            .clone()
            .handle_command("cli", "capped", None, &src, Origin::User)
            .await;
        if let WatchCommandReply::Reply(s) = r {
            assert!(s.starts_with("Watch started: w_"), "watch #{i}: {s}");
        } else {
            panic!("watch #{i}: expected Reply");
        }
    }

    let src = format!("{}", paths[5].display());
    let r = reg
        .clone()
        .handle_command("cli", "capped", None, &src, Origin::User)
        .await;
    match r {
        WatchCommandReply::Reply(s) => {
            assert!(s.contains("limit reached") && s.contains("5/5"), "got: {s}");
        }
        _ => panic!("expected Reply explaining limit"),
    }

    let _ = reg.stop_all_for("cli", "capped").await;
}

#[tokio::test]
async fn stop_removes_dedup_entry_so_restart_is_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restart.log");
    tokio::fs::write(&path, "").await.unwrap();
    let src = format!("{}", path.display());

    let reg = fresh_registry();
    let started = reg
        .clone()
        .handle_command("cli", "carol", None, &src, Origin::User)
        .await;
    let id1 = match started {
        WatchCommandReply::Reply(s) => extract_id(&s),
        _ => panic!("no id"),
    };

    let stop_reply = reg
        .clone()
        .handle_command("cli", "carol", None, &format!("stop {id1}"), Origin::User)
        .await;
    assert!(matches!(stop_reply, WatchCommandReply::Reply(_)));

    // Re-start the same source — should get a fresh id, not the prior one.
    let restarted = reg
        .clone()
        .handle_command("cli", "carol", None, &src, Origin::User)
        .await;
    let id2 = match restarted {
        WatchCommandReply::Reply(s) => extract_id(&s),
        _ => panic!("no id"),
    };
    assert_ne!(id1, id2, "restart should generate fresh id");

    let _ = reg.stop_all_for("cli", "carol").await;
}

fn classify(r: &WatchCommandReply) -> &'static str {
    match r {
        WatchCommandReply::Reply(_) => "Reply",
        WatchCommandReply::Silent => "Silent",
    }
}

fn extract_id(s: &str) -> String {
    s.split_whitespace()
        .find(|w| w.starts_with("w_"))
        .unwrap_or_default()
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
        .to_owned()
}
