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
        pinned: false,
    };
    // First transition to Core should return true.
    assert!(doc.evaluate_tier_transition());
    assert_eq!(doc.tier, MemDocTier::Core);
    // Second call: already Core, should return false.
    assert!(!doc.evaluate_tier_transition());
}

#[test]
fn tier_promotion_via_high_access_alone() {
    let mut doc = MemoryDoc {
        id: "frequent".to_owned(),
        scope: "agent:test".to_owned(),
        kind: "note".to_owned(),
        text: "broadly relevant".to_owned(),
        vector: vec![],
        created_at: 0,
        accessed_at: 0,
        access_count: 15,    // hits path 1
        importance: 0.5,     // mediocre
        tier: MemDocTier::Working,
        abstract_text: None,
        overview_text: None,
        tags: vec![],
        pinned: false,
    };
    assert!(doc.evaluate_tier_transition());
    assert_eq!(doc.tier, MemDocTier::Core);
}

#[test]
fn tier_promotion_via_high_importance_alone() {
    let mut doc = MemoryDoc {
        id: "important".to_owned(),
        scope: "agent:test".to_owned(),
        kind: "note".to_owned(),
        text: "strong positive feedback".to_owned(),
        vector: vec![],
        created_at: 0,
        accessed_at: 0,
        access_count: 2,     // rarely recalled
        importance: 0.95,    // hits path 2
        tier: MemDocTier::Working,
        abstract_text: None,
        overview_text: None,
        tags: vec![],
        pinned: false,
    };
    assert!(doc.evaluate_tier_transition());
    assert_eq!(doc.tier, MemDocTier::Core);
}

#[test]
fn tier_no_promotion_when_below_all_paths() {
    let mut doc = MemoryDoc {
        id: "marginal".to_owned(),
        scope: "agent:test".to_owned(),
        kind: "note".to_owned(),
        text: "not enough yet".to_owned(),
        vector: vec![],
        created_at: 0,
        accessed_at: 0,
        access_count: 4,     // <5 (fails path 3) and <15 (fails path 1)
        importance: 0.85,    // <0.9 (fails path 2), >=0.8 but access too low for path 3
        tier: MemDocTier::Working,
        abstract_text: None,
        overview_text: None,
        tags: vec![],
        pinned: false,
    };
    assert!(!doc.evaluate_tier_transition());
    assert_ne!(doc.tier, MemDocTier::Core);
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
        pinned: false,
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

// ---------------------------------------------------------------------------
// Crystallizer: find_cluster requires MIN_CLUSTER_SIZE (3)
// ---------------------------------------------------------------------------

use rsclaw::skill::crystallizer::{build_distill_prompt, write_skill};

#[test]
fn build_distill_prompt_contains_cluster_texts() {
    let docs: Vec<MemoryDoc> = (0..3)
        .map(|i| MemoryDoc {
            id: format!("c{i}"),
            scope: "agent:test".to_owned(),
            kind: "note".to_owned(),
            text: format!("Memory about topic X, variant {i}"),
            vector: vec![],
            created_at: 0,
            accessed_at: 0,
            access_count: 12,
            importance: 0.9,
            tier: MemDocTier::Core,
            abstract_text: None,
            overview_text: None,
            tags: vec![],
            pinned: false,
        })
        .collect();

    let prompt = build_distill_prompt(&docs);
    // Prompt should contain all 3 memories
    assert!(prompt.contains("variant 0"), "prompt missing variant 0");
    assert!(prompt.contains("variant 1"), "prompt missing variant 1");
    assert!(prompt.contains("variant 2"), "prompt missing variant 2");
    // Should mention SKILL.md format
    assert!(
        prompt.contains("SKILL.md") || prompt.contains("skill"),
        "prompt should mention skill format"
    );
}

#[test]
fn write_skill_creates_file() {
    let dir = std::env::temp_dir().join("rsclaw-test-skills");
    let _ = std::fs::remove_dir_all(&dir);

    let content = "---\nname: test-skill\ndescription: A test\nversion: 1.0.0\n---\nStep 1: do something";
    let path = write_skill(&dir, "test-skill", content).expect("write_skill");

    assert!(path.exists(), "SKILL.md should exist");
    let read = std::fs::read_to_string(&path).expect("read skill");
    assert!(read.contains("test-skill"), "content should contain name");
    assert!(read.contains("Step 1"), "content should contain steps");

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Meditation: config defaults
// ---------------------------------------------------------------------------

use rsclaw::heartbeat::meditation::MeditationConfig;

#[test]
fn meditation_config_defaults() {
    let cfg = MeditationConfig::default();
    assert!((cfg.dedup_threshold - 0.92).abs() < 0.01);
    assert_eq!(cfg.batch_size, 50);
    assert_eq!(cfg.crystallized_ttl_days, 7);
}
