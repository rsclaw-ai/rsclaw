//! Frozen baseline for the `rsclaw/2026.5.5` wire prefix.
//!
//! `tests/fixtures/baseline-2026.5.5.json` captures the byte-exact
//! `shared_prefix` + `builtin_tools` array the gateway sends as
//! `dynamic_prefix.system` + `dynamic_prefix.tools` for this version.
//! These are the two fields that participate in the worker-side base
//! layer KV cache hash, so any drift here means existing rsclaw-llm
//! base layer slots stop being reusable across hosts running the same
//! gateway version — defeating the static-prefix-cache reuse design.
//!
//! When this test fails:
//!   1. If the change was UNINTENTIONAL: revert the code that produced
//!      the drift. shared_prefix changes should only happen on a
//!      gateway version bump; builtin_tools content/order changes need
//!      explicit justification.
//!   2. If the change was INTENTIONAL (you bumped the gateway version
//!      or added/restructured a builtin tool on purpose): regenerate
//!      the fixture with
//!          cargo build --release --bin rsclaw
//!          target/release/rsclaw debug dump-prompt-spec --json
//!              | jq '{rsclaw_version, shared_prefix, builtin_tools}'
//!              > tests/fixtures/baseline-2026.5.5.json
//!      and re-add the `_doc` header that lives at the top of the
//!      fixture (preserved for human readers).
//!
//! Coordination with rsclaw-llm:
//!   The SHA-256s of the two byte-exact fields ARE the canonical
//!   identifier the worker should use when ingesting `rsclaw/2026.5.5`
//!   into its static prefix registry. If this test passes locally and
//!   the worker's pre-registered KV doesn't hit on traffic from this
//!   gateway, the worker registry is stale — re-ingest from the
//!   fixture.

use std::path::PathBuf;

use rsclaw::agent::prompt_builder::{BUILTIN_TOOL_NAMES, build_shared_system_prefix};
use rsclaw::agent::tools_builder::build_tool_list;
use rsclaw::provider::ToolDef;
use rsclaw::skill::SkillRegistry;
use rsclaw::skill::manifest::SkillManifest;
use serde_json::Value;
use sha2::{Digest, Sha256};

const FIXTURE_PATH: &str = "tests/fixtures/baseline-2026.5.5.json";

fn load_baseline() -> Value {
    let bytes = std::fs::read(FIXTURE_PATH).unwrap_or_else(|e| {
        panic!("failed to read {FIXTURE_PATH}: {e}");
    });
    serde_json::from_slice(&bytes).expect("fixture must be valid JSON")
}

/// SkillRegistry with a single placeholder skill — just enough to
/// trigger `use_skill` registration in `build_tool_list`. The skill's
/// name and description must NOT appear in any tool's bytes (verified
/// implicitly: if they did, the fixture comparison would fail because
/// the fixture was generated from a different skill set).
fn baseline_skill_registry() -> SkillRegistry {
    let mut reg = SkillRegistry::new();
    reg.insert(SkillManifest {
        name: "_baseline_probe".to_owned(),
        description: Some("placeholder skill used only to trigger use_skill tool registration".to_owned()),
        version: Some("0.0.0".to_owned()),
        requires_rsclaw: None,
        tools: Vec::new(),
        extra: Default::default(),
        dir: PathBuf::from("/dev/null"),
        prompt: String::new(),
    });
    reg
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[test]
fn baseline_rsclaw_version_pinned() {
    let fixture = load_baseline();
    let pinned = fixture["rsclaw_version"]
        .as_str()
        .expect("fixture rsclaw_version is a string");
    assert_eq!(
        pinned, "2026.5.5",
        "baseline-2026.5.5.json file is wired to a different version. \
         Either rename the fixture or update the test."
    );
}

#[test]
fn baseline_shared_prefix_byte_stable() {
    let fixture = load_baseline();
    let expected = fixture["shared_prefix"]
        .as_str()
        .expect("fixture shared_prefix is a string");

    let actual = build_shared_system_prefix();

    assert_eq!(
        actual.len(),
        expected.len(),
        "shared_prefix LENGTH drifted from 2026.5.5 baseline (actual={}, expected={}). \
         If intentional, regenerate the fixture per the module-level docstring.",
        actual.len(),
        expected.len(),
    );
    if actual != expected {
        // Find first differing offset to surface a useful diff message.
        let n = actual.bytes().zip(expected.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        let preview_actual: String = actual.chars().skip(n.saturating_sub(40)).take(120).collect();
        let preview_expected: String = expected.chars().skip(n.saturating_sub(40)).take(120).collect();
        panic!(
            "shared_prefix bytes drifted from 2026.5.5 baseline at offset {n}.\n\
             actual   :  …{preview_actual}…\n\
             expected :  …{preview_expected}…\n\
             Regenerate the fixture per the module-level docstring if the change is intentional."
        );
    }
}

#[test]
fn baseline_builtin_tools_byte_stable() {
    let fixture = load_baseline();
    let expected = fixture["builtin_tools"]
        .as_array()
        .expect("fixture builtin_tools is an array");

    // Build the canonical builtin_tools list under controlled conditions:
    //   - a single placeholder skill (registers use_skill)
    //   - no AgentRegistry (so no per-agent A2A tools)
    //   - no ExternalAgentConfig (so no external agent tools)
    let skills = baseline_skill_registry();
    let all_tools = build_tool_list(&skills, None, "main", &[]);
    let builtin: Vec<&ToolDef> = all_tools
        .iter()
        .filter(|t| BUILTIN_TOOL_NAMES.contains(&t.name.as_str()))
        .collect();
    // Wire-shape JSON for each tool: `{name, description, input_schema}`.
    // `ToolDef`'s default `Serialize` impl spells the third field as
    // `parameters` (the in-memory name); both the rsclaw wire (in
    // `rsclaw.rs::split_request`) and the `rsclaw debug
    // dump-prompt-spec` output rename it to `input_schema` before
    // serializing. The fixture was produced by the debug command, so
    // mirror that rename here — otherwise the byte comparison fails
    // purely on field naming and the test gives a misleading diff.
    let actual: Value = Value::Array(
        builtin
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect(),
    );

    let expected_value = Value::Array(expected.clone());

    if actual != expected_value {
        // Pinpoint which tool diverged so the failure message is actionable.
        let actual_arr = actual.as_array().expect("actual is array");
        let actual_names: Vec<&str> = actual_arr
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        let expected_names: Vec<&str> = expected
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();

        if actual_names != expected_names {
            panic!(
                "builtin_tools NAME LIST drifted from 2026.5.5 baseline.\n\
                 actual   : {actual_names:?}\n\
                 expected : {expected_names:?}\n\
                 Regenerate the fixture per the module-level docstring if the change is intentional."
            );
        }

        let mut diff_names = Vec::new();
        for (a, e) in actual_arr.iter().zip(expected.iter()) {
            if a != e {
                if let Some(n) = a.get("name").and_then(|n| n.as_str()) {
                    diff_names.push(n.to_owned());
                }
            }
        }
        panic!(
            "builtin_tools CONTENT drifted from 2026.5.5 baseline (names matched, \
             but at least one tool's body diverged): {diff_names:?}.\n\
             Regenerate the fixture per the module-level docstring if the change is intentional."
        );
    }

    assert_eq!(
        builtin.len(),
        37,
        "Expected 37 builtin tools in the 2026.5.5 baseline; got {}. \
         If a builtin tool was added or removed intentionally, regenerate the fixture.",
        builtin.len()
    );
}

#[test]
fn baseline_prefix_id_and_metadata_pinned() {
    let fixture = load_baseline();
    assert_eq!(
        fixture["prefix_id"].as_str(),
        Some("rsclaw/2026.5.5"),
        "fixture's prefix_id field must match the wire identifier the gateway sends"
    );
    assert_eq!(
        fixture["ingested"].as_bool(),
        Some(true),
        "fixture documents a baseline that IS ingested into the worker's static registry"
    );
    let toks = fixture["n_prefix_tokens"]
        .as_u64()
        .expect("n_prefix_tokens is an integer");
    assert!(
        toks > 0,
        "n_prefix_tokens must be a positive estimate; got {toks}"
    );
}

#[test]
fn baseline_version_hash_matches_live_build() {
    // End-to-end fingerprint check: compute SHA-256 of the byte-exact
    // base-layer content the live build emits, then assert it equals
    // the `version_hash` baked into the fixture. This is a single
    // assertion that subsumes the shared_prefix + builtin_tools byte
    // checks; the dedicated tests above remain for actionable diff
    // messages on failure.
    //
    // Hash recipe (must match the regen script in the fixture's _doc):
    //   sha256( shared_prefix.utf8 ||
    //           b"\n" ||
    //           canonical_json(builtin_tools).utf8 )
    //
    // canonical_json = JSON with keys sorted, no whitespace between
    // tokens, ensure_ascii=False (preserves UTF-8 of skill/tool text).

    let fixture = load_baseline();
    let expected_hash = fixture["version_hash"]
        .as_str()
        .expect("fixture version_hash is a string")
        .to_owned();

    // Live shared_prefix.
    let shared_prefix = build_shared_system_prefix();

    // Live builtin_tools — same setup as the byte-stable test.
    let skills = baseline_skill_registry();
    let all_tools = build_tool_list(&skills, None, "main", &[]);
    let builtin: Vec<&ToolDef> = all_tools
        .iter()
        .filter(|t| BUILTIN_TOOL_NAMES.contains(&t.name.as_str()))
        .collect();
    let tools_json: Value = Value::Array(
        builtin
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect(),
    );
    // Canonical-key sort, compact (no whitespace). Mirrors Python's
    // `json.dumps(..., sort_keys=True, separators=(',', ':'))` which
    // the fixture regen script uses.
    let tools_canonical = canonical_json(&tools_json);

    let mut hasher = Sha256::new();
    hasher.update(shared_prefix.as_bytes());
    hasher.update(b"\n");
    hasher.update(tools_canonical.as_bytes());
    let actual_hash = format!("{:x}", hasher.finalize());

    assert_eq!(
        actual_hash, expected_hash,
        "version_hash drifted from 2026.5.5 baseline.\n\
         actual   = {actual_hash}\n\
         expected = {expected_hash}\n\
         Either shared_prefix or builtin_tools changed bytes. Run the\n\
         other tests in this file for a localised diff; regenerate the\n\
         fixture per the module-level docstring if the change was intentional."
    );
}

/// Serialize `v` to JSON with keys sorted recursively and no whitespace.
/// Mirrors Python's `json.dumps(v, sort_keys=True, ensure_ascii=False,
/// separators=(',', ':'))`. Used so the version_hash recipe stays
/// consistent between the fixture regen script (Python) and this test
/// (Rust).
fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut s = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push('"');
                s.push_str(&k.replace('\\', "\\\\").replace('"', "\\\""));
                s.push_str("\":");
                s.push_str(&canonical_json(&map[*k]));
            }
            s.push('}');
            s
        }
        Value::Array(arr) => {
            let mut s = String::from("[");
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&canonical_json(item));
            }
            s.push(']');
            s
        }
        // serde_json::to_string already produces JSON-compliant output
        // for scalars; reuse it for strings, numbers, bool, null.
        _ => serde_json::to_string(v).expect("scalar serializable"),
    }
}
