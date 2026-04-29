//! Integration test: load the shell_plugin_echo fixture and verify the
//! bidirectional protocol round-trips a tool_call request through the
//! shell-bridge JSON-RPC layer.

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test(flavor = "current_thread")]
async fn shell_plugin_echo_tool_dispatches() {
    let fixtures_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/shell_plugin_echo");

    let manifest = rsclaw::plugin::load_manifest(&fixtures_dir).expect("parse manifest");

    let browser: Arc<Mutex<Option<rsclaw::browser::BrowserSession>>> =
        Arc::new(Mutex::new(None));
    let host_dispatch = Arc::new(
        rsclaw::plugin::host_methods::HostMethodRegistry::new(None, browser),
    );

    let plugin = rsclaw::plugin::Plugin::spawn(manifest, host_dispatch)
        .await
        .expect("spawn plugin");

    let result = plugin
        .call(
            "tool_call",
            serde_json::json!({
                "tool": "echo",
                "args": { "msg": "hello" },
                "_ctx": { "target_id": "t", "channel": "test", "session_key": "s" }
            }),
        )
        .await
        .expect("call");

    assert_eq!(result["echoed"]["msg"], "hello");

    plugin.shutdown().await;
}
