//! Integration tests for DmPolicy enforcement via `DmPolicyEnforcer`.
//!
//! All tests are purely in-memory — no channels, no network, no LLM.

#![allow(unused)]

use rsclaw::{
    channel::{DmPolicyEnforcer, PolicyResult},
    config::schema::DmPolicy,
};

// ---------------------------------------------------------------------------
// test_dm_policy_open
//
// In Open mode, any peer_id is allowed unconditionally.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_dm_policy_open() {
    let enforcer = DmPolicyEnforcer::new(DmPolicy::Open, vec![]);

    // Random users must all be allowed.
    for peer in ["alice", "bob", "0123456789", "unknown_user_99"] {
        let result = enforcer.check(peer).await;
        assert_eq!(
            result,
            PolicyResult::Allow,
            "Open policy should allow '{peer}', got: {result:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// test_dm_policy_allowlist
//
// In Allowlist mode, only peers in `allow_from` are permitted.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_dm_policy_allowlist() {
    let allowed = vec!["alice".to_owned(), "charlie".to_owned()];
    let enforcer = DmPolicyEnforcer::new(DmPolicy::Allowlist, allowed);

    // Allowed peers.
    assert_eq!(
        enforcer.check("alice").await,
        PolicyResult::Allow,
        "alice is in allowlist"
    );
    assert_eq!(
        enforcer.check("charlie").await,
        PolicyResult::Allow,
        "charlie is in allowlist"
    );

    // Denied peers.
    for denied in ["bob", "dave", "ALICE", "  alice  ", ""] {
        let result = enforcer.check(denied).await;
        assert_eq!(
            result,
            PolicyResult::Deny,
            "'{denied}' should be denied, got: {result:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// test_dm_policy_allowlist_wildcard
//
// A single "*" entry in allow_from makes the allowlist behave like Open.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_dm_policy_allowlist_wildcard() {
    let enforcer = DmPolicyEnforcer::new(DmPolicy::Allowlist, vec!["*".to_owned()]);

    for peer in ["alice", "bob", "anyone"] {
        assert_eq!(
            enforcer.check(peer).await,
            PolicyResult::Allow,
            "wildcard allowlist should allow '{peer}'"
        );
    }
}

// ---------------------------------------------------------------------------
// test_dm_policy_disabled
//
// In Disabled mode, every peer is denied — regardless of peer_id content.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_dm_policy_disabled() {
    // Even if allow_from is populated, Disabled should still deny.
    let enforcer =
        DmPolicyEnforcer::new(DmPolicy::Disabled, vec!["alice".to_owned(), "*".to_owned()]);

    for peer in ["alice", "bob", "admin", ""] {
        let result = enforcer.check(peer).await;
        assert_eq!(
            result,
            PolicyResult::Deny,
            "Disabled policy should deny '{peer}', got: {result:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// test_dm_policy_pairing_issues_code
//
// In Pairing mode, an unknown peer gets a pairing code on first contact.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_dm_policy_pairing_issues_code() {
    let enforcer = DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]);

    let result = enforcer.check("new_user").await;
    assert!(
        matches!(result, PolicyResult::SendPairingCode(_)),
        "first contact in Pairing mode should yield a code, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// test_dm_policy_pairing_approved_allows
//
// After the code is approved, subsequent checks for the same peer return Allow.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_dm_policy_pairing_approved_allows() {
    let enforcer = DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]);

    // Get a pairing code.
    let code = match enforcer.check("peer_42").await {
        PolicyResult::SendPairingCode(c) => c,
        other => panic!("expected SendPairingCode, got {other:?}"),
    };

    // Approve it.
    let approved_peer = enforcer.approve_pairing(&code).await;
    assert_eq!(
        approved_peer.as_deref(),
        Some("peer_42"),
        "approve_pairing should return the peer ID"
    );

    // Now the peer is approved — check must return Allow.
    assert_eq!(
        enforcer.check("peer_42").await,
        PolicyResult::Allow,
        "approved peer should be allowed on subsequent checks"
    );
}

// ---------------------------------------------------------------------------
// test_dm_policy_pairing_revoke
//
// After revoking an approved peer, they must be denied again (new code issued).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_dm_policy_pairing_revoke() {
    let enforcer = DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]);

    // Approve "peer_rev".
    let code = match enforcer.check("peer_rev").await {
        PolicyResult::SendPairingCode(c) => c,
        other => panic!("expected code, got {other:?}"),
    };
    enforcer.approve_pairing(&code).await;
    assert_eq!(enforcer.check("peer_rev").await, PolicyResult::Allow);

    // Revoke the approval.
    enforcer.revoke("peer_rev").await;

    // Next check must issue a new pairing code (not Allow).
    let after_revoke = enforcer.check("peer_rev").await;
    assert!(
        matches!(after_revoke, PolicyResult::SendPairingCode(_)),
        "revoked peer should require re-pairing, got: {after_revoke:?}"
    );
}

// ---------------------------------------------------------------------------
// test_dm_policy_pairing_wrong_code_rejected
//
// Approving with the wrong code must return None and not grant access.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_dm_policy_pairing_wrong_code_rejected() {
    let enforcer = DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]);

    // Initiate pairing for a user.
    let _ = enforcer.check("peer_x").await;

    // Attempt to approve with an incorrect code.
    let result = enforcer.approve_pairing("0000-0000").await;
    assert!(result.is_none(), "wrong code should not be approved");

    // Peer must still not be allowed (will get another code or queue-full).
    let check = enforcer.check("peer_x").await;
    assert_ne!(
        check,
        PolicyResult::Allow,
        "peer_x should not be allowed after failed approval attempt"
    );
}

// ---------------------------------------------------------------------------
// test_pairing_code_case_insensitive
//
// Approving with a lowercase version of the code should work.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_pairing_code_case_insensitive() {
    let enforcer = DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]);

    let code = match enforcer.check("peer_ci").await {
        PolicyResult::SendPairingCode(c) => c,
        other => panic!("expected SendPairingCode, got {other:?}"),
    };

    // Approve with the code converted to lowercase.
    let lower_code = code.to_lowercase();
    let approved = enforcer.approve_pairing(&lower_code).await;
    assert_eq!(
        approved.as_deref(),
        Some("peer_ci"),
        "approve should succeed with lowercase code"
    );

    assert_eq!(
        enforcer.check("peer_ci").await,
        PolicyResult::Allow,
        "peer should be allowed after case-insensitive approval"
    );
}

// ---------------------------------------------------------------------------
// test_same_peer_reuses_existing_code
//
// Checking the same peer twice should return the same pairing code.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_same_peer_reuses_existing_code() {
    let enforcer = DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]);

    let code1 = match enforcer.check("repeat_user").await {
        PolicyResult::SendPairingCode(c) => c,
        other => panic!("expected code, got {other:?}"),
    };

    let code2 = match enforcer.check("repeat_user").await {
        PolicyResult::SendPairingCode(c) => c,
        other => panic!("expected code, got {other:?}"),
    };

    assert_eq!(
        code1, code2,
        "same peer should receive the same pairing code"
    );
}

// ---------------------------------------------------------------------------
// test_pairing_code_character_set
//
// Generate many codes and verify all characters are from the valid charset.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_pairing_code_character_set() {
    const VALID_CHARS: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

    // Generate codes by creating fresh enforcers (each has its own store).
    for i in 0..100 {
        let enforcer = DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]);
        let code = match enforcer.check(&format!("user_{i}")).await {
            PolicyResult::SendPairingCode(c) => c,
            other => panic!("expected code, got {other:?}"),
        };

        // Format: XXXX-XXXX
        let parts: Vec<&str> = code.split('-').collect();
        assert_eq!(parts.len(), 2, "code should have 2 parts: {code}");
        assert_eq!(parts[0].len(), 4, "first part should be 4 chars: {code}");
        assert_eq!(parts[1].len(), 4, "second part should be 4 chars: {code}");

        for ch in code.chars() {
            if ch == '-' {
                continue;
            }
            assert!(
                VALID_CHARS.contains(ch),
                "character '{ch}' in code '{code}' is not in valid charset"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// test_allowlist_empty_denies_all
//
// An empty allowlist should deny every peer.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_allowlist_empty_denies_all() {
    let enforcer = DmPolicyEnforcer::new(DmPolicy::Allowlist, vec![]);

    for peer in ["alice", "bob", "admin", "", "root"] {
        assert_eq!(
            enforcer.check(peer).await,
            PolicyResult::Deny,
            "empty allowlist should deny '{peer}'"
        );
    }
}

// ---------------------------------------------------------------------------
// test_concurrent_pairing_requests
//
// 10 concurrent tasks request pairing; at most 3 codes should be issued
// (MAX_PENDING_PAIRINGS = 3), the rest get PairingQueueFull.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_pairing_requests() {
    use std::sync::Arc;

    let enforcer = Arc::new(DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]));

    let mut handles = Vec::new();
    for i in 0..10 {
        let e = Arc::clone(&enforcer);
        handles.push(tokio::spawn(async move {
            e.check(&format!("concurrent_user_{i}")).await
        }));
    }

    let mut codes = 0u32;
    let mut fulls = 0u32;
    for h in handles {
        match h.await.unwrap() {
            PolicyResult::SendPairingCode(_) => codes += 1,
            PolicyResult::PairingQueueFull => fulls += 1,
            other => panic!("unexpected result: {other:?}"),
        }
    }

    assert_eq!(codes, 3, "exactly 3 pairing codes should be issued");
    assert_eq!(fulls, 7, "remaining 7 should get PairingQueueFull");
}

// ---------------------------------------------------------------------------
// test_concurrent_approve_same_code
//
// Two concurrent approvals of the same code: exactly 1 should succeed.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_concurrent_approve_same_code() {
    use std::sync::Arc;

    let enforcer = Arc::new(DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]));

    let code = match enforcer.check("dup_approve_user").await {
        PolicyResult::SendPairingCode(c) => c,
        other => panic!("expected code, got {other:?}"),
    };

    let e1 = Arc::clone(&enforcer);
    let e2 = Arc::clone(&enforcer);
    let c1 = code.clone();
    let c2 = code.clone();

    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { e1.approve_pairing(&c1).await }),
        tokio::spawn(async move { e2.approve_pairing(&c2).await }),
    );

    let results = [r1.unwrap(), r2.unwrap()];
    let successes = results.iter().filter(|r| r.is_some()).count();
    assert_eq!(
        successes, 1,
        "exactly one concurrent approval should succeed, got {successes}"
    );

    // The peer should be approved regardless.
    assert_eq!(
        enforcer.check("dup_approve_user").await,
        PolicyResult::Allow,
        "peer should be allowed after approval"
    );
}
