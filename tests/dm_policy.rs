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
