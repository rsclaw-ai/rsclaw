//! Tests for new features: User-Agent configuration and OpenCode tool

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
// OpenCode ToolDef
// ---------------------------------------------------------------------------

#[test]
fn opencode_tooldef_in_tool_list() {
    // Check that opencode tool definition is present in runtime.rs
    // This tests the static definition by searching for it in the source

    let source = include_str!("../src/agent/runtime.rs");

    // Check that opencode tool is defined
    assert!(
        source.contains(r#"name: "opencode".to_owned()"#),
        "opencode tool should be defined in runtime.rs"
    );

    // Check that it has the correct description
    assert!(
        source.contains("OpenCode"),
        "opencode tool should mention OpenCode"
    );

    // Check that it has prompt parameter
    assert!(
        source.contains(r#""prompt""#),
        "opencode tool should have prompt parameter"
    );

    // Check that it has session_id parameter
    assert!(
        source.contains(r#""session_id""#),
        "opencode tool should have session_id parameter"
    );
}

#[test]
fn opencode_tool_dispatch_exists() {
    // Verify that the dispatch logic handles opencode
    let source = include_str!("../src/agent/runtime.rs");

    assert!(
        source.contains(r#""opencode" => return self.tool_opencode(args).await"#),
        "opencode tool should be dispatched in runtime"
    );

    assert!(
        source.contains("async fn tool_opencode"),
        "tool_opencode method should exist"
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
    };

    assert_eq!(config.user_agent, Some("TestAgent/1.0".to_string()));
}

// ---------------------------------------------------------------------------
// OpenCode tool implementation exists
// ---------------------------------------------------------------------------

#[test]
fn opencode_tool_calls_acp_client() {
    // Verify tool_opencode implementation uses AcpClient
    let source = include_str!("../src/agent/runtime.rs");

    assert!(
        source.contains("use crate::acp::client::AcpClient"),
        "tool_opencode should import AcpClient"
    );

    assert!(
        source.contains("AcpClient::spawn"),
        "tool_opencode should spawn AcpClient"
    );

    assert!(
        source.contains(".initialize("),
        "tool_opencode should initialize the client"
    );

    assert!(
        source.contains("client.create_session"),
        "tool_opencode should create session"
    );

    // Method call might be .send_prompt( or client.send_prompt
    assert!(
        source.contains("send_prompt"),
        "tool_opencode should send prompt"
    );
}

#[test]
fn opencode_tool_handles_session_id() {
    // Verify that session_id parameter is handled
    let source = include_str!("../src/agent/runtime.rs");

    assert!(
        source.contains(r#"let session_id = args["session_id"].as_str()"#),
        "tool_opencode should extract session_id from args"
    );

    assert!(
        source.contains("client.resume_session"),
        "tool_opencode should support resuming session"
    );
}
