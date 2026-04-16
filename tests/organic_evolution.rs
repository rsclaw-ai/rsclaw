//! Tests for the organic evolution system: feedback loop, crystallization,
//! and meditation.

// ---------------------------------------------------------------------------
// Memory: adjust_importance clamping
// ---------------------------------------------------------------------------

use rsclaw::agent::memory::{MemDocTier, MemoryDoc};

#[test]
fn tier_promotion_returns_true_on_first_core() {
    let mut doc = MemoryDoc {
        id: "d1".to_owned(),
        scope: "agent:test".to_owned(),
        kind: "note".to_owned(),
        text: "test".to_owned(),
        vector: vec![],
        created_at: 0,
        accessed_at: 0,
        access_count: 10,
        importance: 0.85,
        tier: MemDocTier::Working,
        abstract_text: None,
        overview_text: None,
        tags: vec![],
    };
    // First transition to Core should return true.
    assert!(doc.evaluate_tier_transition());
    assert_eq!(doc.tier, MemDocTier::Core);
    // Second call: already Core, should return false.
    assert!(!doc.evaluate_tier_transition());
}

#[test]
fn tier_demotion_does_not_return_true() {
    let mut doc = MemoryDoc {
        id: "d2".to_owned(),
        scope: "agent:test".to_owned(),
        kind: "note".to_owned(),
        text: "old".to_owned(),
        vector: vec![],
        created_at: 0, // very old
        accessed_at: 0,
        access_count: 0,
        importance: 0.1,
        tier: MemDocTier::Working,
        abstract_text: None,
        overview_text: None,
        tags: vec![],
    };
    assert!(!doc.evaluate_tier_transition());
    assert_eq!(doc.tier, MemDocTier::Peripheral);
}

// ---------------------------------------------------------------------------
// Heartbeat: meditate type parsing
// ---------------------------------------------------------------------------

use rsclaw::heartbeat::{parse_heartbeat_md, HeartbeatType};

#[test]
fn parse_meditate_type() {
    let raw = "---\nevery: 6h\ntype: meditate\n---\nRun memory maintenance.";
    let spec = parse_heartbeat_md(raw).expect("parse meditate heartbeat");
    assert_eq!(spec.spec_type, HeartbeatType::Meditate);
    assert!(spec.content.contains("memory maintenance"));
}

#[test]
fn parse_meditation_type_alias() {
    let raw = "---\nevery: 4h\ntype: meditation\n---\nCleanup.";
    let spec = parse_heartbeat_md(raw).expect("parse meditation heartbeat");
    assert_eq!(spec.spec_type, HeartbeatType::Meditate);
}

#[test]
fn parse_default_type_is_message() {
    let raw = "---\nevery: 30m\n---\nCheck health.";
    let spec = parse_heartbeat_md(raw).expect("parse default heartbeat");
    assert_eq!(spec.spec_type, HeartbeatType::Message);
}

// ---------------------------------------------------------------------------
// Crystallizer: slugify
// ---------------------------------------------------------------------------

use rsclaw::skill::crystallizer::slugify;

#[test]
fn slugify_basic() {
    assert_eq!(slugify("Web Search Pattern"), "web-search-pattern");
}

#[test]
fn slugify_special_chars() {
    assert_eq!(slugify("file/download (v2)"), "file-download-v2");
}

#[test]
fn slugify_already_clean() {
    assert_eq!(slugify("my-skill"), "my-skill");
}

#[test]
fn slugify_empty() {
    assert_eq!(slugify(""), "unnamed-skill");
}
