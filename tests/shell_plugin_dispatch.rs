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

#[tokio::test(flavor = "current_thread")]
async fn shell_plugin_notify_reaches_host() {
    let fixtures_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/shell_plugin_echo");

    let manifest = rsclaw::plugin::load_manifest(&fixtures_dir).expect("parse manifest");

    let browser: Arc<Mutex<Option<rsclaw::browser::BrowserSession>>> =
        Arc::new(Mutex::new(None));
    let (notify_tx, mut notify_rx) =
        tokio::sync::broadcast::channel::<rsclaw::channel::OutboundMessage>(16);

    let host_dispatch = Arc::new(
        rsclaw::plugin::host_methods::HostMethodRegistry::new(Some(notify_tx), browser),
    );

    let plugin = rsclaw::plugin::Plugin::spawn(manifest, host_dispatch)
        .await
        .expect("spawn plugin");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        plugin.call(
            "tool_call",
            serde_json::json!({
                "tool": "notify_then_echo",
                "args": { "msg": "ping" },
                "_ctx": { "target_id": "t1", "channel": "test", "session_key": "s1" }
            }),
        ),
    )
    .await
    .expect("call did not time out")
    .expect("call ok");

    assert_eq!(result["notified"], serde_json::json!(true));
    assert_eq!(result["echoed"]["msg"], "ping");

    let received = notify_rx
        .try_recv()
        .expect("notify message should have arrived on the broadcast channel");
    assert_eq!(received.target_id, "t1");
    assert_eq!(received.channel.as_deref(), Some("test"));
    assert!(
        received.text.contains("notify: ping"),
        "expected text to contain 'notify: ping', got: {}",
        received.text
    );

    plugin.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn shell_plugin_legacy_hook_call_still_works() {
    let fixtures_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/shell_plugin_echo");

    let manifest = rsclaw::plugin::load_manifest(&fixtures_dir).expect("parse manifest");

    let browser: Arc<Mutex<Option<rsclaw::browser::BrowserSession>>> =
        Arc::new(Mutex::new(None));
    let host_dispatch = Arc::new(
        // No notify_tx — simulates the pre-bidirectional hook context where
        // shell plugins were one-way callers only.
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
                "args": { "msg": "legacy" },
                "_ctx": {}
            }),
        )
        .await
        .expect("legacy call");

    assert_eq!(result["echoed"]["msg"], "legacy");

    plugin.shutdown().await;
}
