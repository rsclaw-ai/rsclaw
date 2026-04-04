//! Integration tests for the config loader and validator.
//!
//! Uses `tempfile` to write temporary JSON5 config files and verifies that
//! valid configs load successfully while invalid configs are rejected.

use std::io::Write;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write a JSON5 string to a temp file and return the path.
fn write_temp_json5(content: &str) -> (tempfile::NamedTempFile, std::path::PathBuf) {
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    f.write_all(content.as_bytes()).expect("write");
    let path = f.path().to_owned();
    (f, path)
}

// ---------------------------------------------------------------------------
// Valid config tests
// ---------------------------------------------------------------------------

#[test]
fn minimal_valid_config_loads() {
    let json5 = r#"{
        gateway: { port: 8080 },
        agents: { list: [{ id: "main", default: true }] },
    }"#;
    let (_f, path) = write_temp_json5(json5);
    let result = rsclaw::config::load_from(path);
    assert!(result.is_ok(), "minimal config should load: {:?}", result);
}

#[test]
fn config_with_auth_token_loads() {
    let json5 = r#"{
        gateway: {
            port: 9000,
            bind: "all",
            auth: { token: "my-secret" },
        },
        agents: { list: [{ id: "assistant" }] },
    }"#;
    let (_f, path) = write_temp_json5(json5);
    let result = rsclaw::config::load_from(path);
    assert!(
        result.is_ok(),
        "config with auth token should load: {:?}",
        result
    );
    let cfg = result.unwrap();
    assert_eq!(cfg.gateway.auth_token.as_deref(), Some("my-secret"));
}

#[test]
fn config_gateway_port_is_set() {
    let json5 = r#"{ gateway: { port: 7777 } }"#;
    let (_f, path) = write_temp_json5(json5);
    let cfg = rsclaw::config::load_from(path).expect("should load");
    assert_eq!(cfg.gateway.port, 7777);
}

#[test]
fn config_with_model_provider_loads() {
    let json5 = r#"{
        models: {
            providers: {
                anthropic: {
                    apiKey: "sk-ant-test",
                    enabled: true,
                }
            }
        }
    }"#;
    let (_f, path) = write_temp_json5(json5);
    let result = rsclaw::config::load_from(path);
    assert!(
        result.is_ok(),
        "config with model provider should load: {:?}",
        result
    );
}

#[test]
fn config_with_cron_enabled_loads() {
    let json5 = r#"{
        cron: {
            enabled: true,
            jobs: [
                { id: "daily", schedule: "0 9 * * *", message: "Good morning" }
            ]
        }
    }"#;
    let (_f, path) = write_temp_json5(json5);
    let result = rsclaw::config::load_from(path);
    assert!(result.is_ok(), "config with cron should load: {:?}", result);
}

// ---------------------------------------------------------------------------
// Invalid config tests
// ---------------------------------------------------------------------------

#[test]
fn duplicate_agent_ids_are_rejected() {
    let json5 = r#"{
        agents: {
            list: [
                { id: "alpha", default: true },
                { id: "alpha" },
            ]
        }
    }"#;
    let (_f, path) = write_temp_json5(json5);
    let result = rsclaw::config::load_from(path);
    assert!(
        result.is_err(),
        "duplicate agent ids should fail validation"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("duplicate") || msg.contains("alpha"),
        "error message should mention the duplicate: {msg}"
    );
}

#[test]
fn multiple_default_agents_are_rejected() {
    let json5 = r#"{
        agents: {
            list: [
                { id: "alpha", default: true },
                { id: "beta",  default: true },
            ]
        }
    }"#;
    let (_f, path) = write_temp_json5(json5);
    let result = rsclaw::config::load_from(path);
    assert!(
        result.is_err(),
        "multiple default agents should fail validation"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("default"),
        "error message should mention 'default': {msg}"
    );
}

#[test]
fn invalid_json5_syntax_returns_error() {
    // Missing closing brace — parse should fail.
    let json5 = r#"{ gateway: { port: 8080 "#;
    let (_f, path) = write_temp_json5(json5);
    let result = rsclaw::config::load_from(path);
    assert!(result.is_err(), "invalid JSON5 should return an error");
}

#[test]
fn unknown_field_in_config_is_tolerated() {
    // Unknown fields are silently ignored for OpenClaw compatibility.
    let json5 = r#"{ notAValidField: true }"#;
    let (_f, path) = write_temp_json5(json5);
    let result = rsclaw::config::load_from(path);
    assert!(
        result.is_ok(),
        "unknown fields should be tolerated: {:?}",
        result
    );
}
