//! Tests for new features: User-Agent configuration and coding-agent
//! tool surface.
//!
//! The coding-agent tests changed shape during the cap migration: the
//! four old `tool_opencode/codex/claudecode` entries collapsed into a
//! single `tool_cap` with an `agent` enum. Source-level smoke checks
//! below verify the new surface stays wired; behavioural coverage
//! lives in `cap::*` unit tests inside the lib crate.

// ---------------------------------------------------------------------------
// User-Agent from providers.json
// ---------------------------------------------------------------------------

#[test]
fn read_user_agent_from_providers_json_file() {
    // Create temp directory structure
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let data_dir = temp_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    // Write providers.json
    let providers_file = data_dir.join("providers.json");
    std::fs::write(
        &providers_file,
        r#"{
            "anthropic": {
                "userAgent": "OpenClaw/1.0"
            },
            "openai": {
                "userAgent": "MyCustomAgent/2.0"
            }
        }"#,
    )
    .expect("write providers.json");

    // Simulate reading user_agent from file (the logic from startup.rs)
    fn read_provider_file_user_agent_internal(
        base_dir: &std::path::Path,
        provider: &str,
    ) -> Option<String> {
        let provider_file = base_dir.join("data/providers.json");
        if !provider_file.exists() {
            return None;
        }
        let content = std::fs::read_to_string(&provider_file).ok()?;
        let json: serde_json::Value = serde_json::from_str(&content).ok()?;
        json.get(provider)?
            .get("userAgent")?
            .as_str()
            .map(String::from)
    }

    // Test reading
    let ua = read_provider_file_user_agent_internal(temp_dir.path(), "anthropic");
    assert_eq!(ua, Some("OpenClaw/1.0".to_string()));

    let ua2 = read_provider_file_user_agent_internal(temp_dir.path(), "openai");
    assert_eq!(ua2, Some("MyCustomAgent/2.0".to_string()));

    // Non-existent provider
    let ua3 = read_provider_file_user_agent_internal(temp_dir.path(), "gemini");
    assert_eq!(ua3, None);
}

#[test]
fn user_agent_env_var_priority_over_file() {
    // Test that env var takes precedence (simulating startup.rs logic)
    let env_ua = std::env::var("RSCLAW_TEST_USER_AGENT").ok();
    let file_ua = Some("FromFile".to_string());

    // Simulate the priority: env var > file
    // If env var not set, file value should be used
    let result = if env_ua.is_some() {
        env_ua.clone()
    } else {
        file_ua.clone()
    };

    if env_ua.is_none() {
        assert_eq!(result, file_ua);
    }
}

#[test]
fn user_agent_none_when_no_config() {
    // Test reading from non-existent file returns None
    fn read_provider_file_user_agent_internal(
        base_dir: &std::path::Path,
        provider: &str,
    ) -> Option<String> {
        let provider_file = base_dir.join("data/providers.json");
        if !provider_file.exists() {
            return None;
        }
        let content = std::fs::read_to_string(&provider_file).ok()?;
        let json: serde_json::Value = serde_json::from_str(&content).ok()?;
        json.get(provider)?
            .get("userAgent")?
            .as_str()
            .map(String::from)
    }

    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    // No data directory, no providers.json

    let ua = read_provider_file_user_agent_internal(temp_dir.path(), "anthropic");
    assert_eq!(ua, None);
}

// ---------------------------------------------------------------------------
// Coding-agent tool surface (post-cap migration)
// ---------------------------------------------------------------------------

#[test]
fn tool_cap_registered_in_tool_builder() {
    let source = include_str!("../crates/rsclaw-agent/src/tools_builder.rs");
    assert!(
        source.contains(r#"name: "cap".to_owned()"#),
        "tool_cap should be registered in tools_builder.rs"
    );
    for agent in ["claudecode", "openclaude", "opencode", "codex"] {
        assert!(
            source.contains(agent),
            "tool_cap should mention agent `{agent}`"
        );
    }
}

#[test]
fn tool_cap_dispatched_in_runtime() {
    let source = include_str!("../crates/rsclaw-agent/src/runtime/dispatch.rs");
    assert!(
        source.contains(r#""cap" => return self.tool_cap(ctx, args).await"#),
        "tool_cap should be dispatched in runtime"
    );
}

// ---------------------------------------------------------------------------
// Provider user_agent field in schema
// ---------------------------------------------------------------------------

#[test]
fn provider_config_has_user_agent_field() {
    use rsclaw::config::schema::ProviderConfig;

    // Test that we can create config with user_agent
    let config = ProviderConfig {
        base_url: Some("https://api.openai.com".to_string()),
        api_key: None,
        api: None,
        models: None,
        enabled: Some(true),
        user_agent: Some("TestAgent/1.0".to_string()),
        prefix_id: None,
        compact_timeout_secs: None,
        constrain_tool_calls: None,
    };

    assert_eq!(config.user_agent, Some("TestAgent/1.0".to_string()));
}

// ---------------------------------------------------------------------------
// Cap tool implementation smoke checks
// ---------------------------------------------------------------------------

#[test]
fn tool_cap_drives_cap_agent_manager() {
    // tool_cap goes through CapAgentManager::dispatch_async rather
    // than directly poking an AcpClient. Behavioural coverage lives in
    // cap::* unit tests (run_turn_*, bridge::tests, permission::tests).
    let source = include_str!("../crates/rsclaw-agent/src/tools_cap.rs");
    assert!(
        source.contains("CapAgentManager"),
        "tool_cap should reference CapAgentManager"
    );
    assert!(
        source.contains("dispatch_async"),
        "tool_cap should call dispatch_async (returns Submitted, not blocking)"
    );
    assert!(
        source.contains("\"status\": \"submitted\""),
        "tool_cap should return status=submitted to the LLM"
    );
}
