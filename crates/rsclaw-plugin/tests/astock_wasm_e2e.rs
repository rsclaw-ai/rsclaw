use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

#[derive(Default)]
struct FakeBackgroundHost {
    cron_calls: AtomicUsize,
    sse_calls: AtomicUsize,
    submit_calls: AtomicUsize,
}

impl rsclaw_plugin::PluginBackgroundHost for FakeBackgroundHost {
    fn cron_register(
        &self,
        plugin: String,
        name: String,
        schedule_json: String,
        ctx: Option<rsclaw_plugin::PluginInvocationContext>,
    ) -> futures::future::BoxFuture<'static, Result<String, String>> {
        self.cron_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            assert_eq!(plugin, "astock");
            assert_eq!(name, "astock.briefing");
            assert!(schedule_json.contains("premarket"));
            assert_eq!(ctx.map(|c| c.channel), Some("test".to_owned()));
            Ok("fake_cron_registered".to_owned())
        })
    }

    fn sse_subscribe(
        &self,
        plugin: String,
        name: String,
        url: String,
        headers_json: String,
        resume_key: String,
        ctx: Option<rsclaw_plugin::PluginInvocationContext>,
    ) -> futures::future::BoxFuture<'static, Result<String, String>> {
        self.sse_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            assert_eq!(plugin, "astock");
            assert_eq!(name, "astock.quick_rally");
            assert!(url.contains("/v1/stream/quick?filter=quick_rally"));
            assert!(headers_json.contains("Bearer test-key"));
            assert!(resume_key.contains("quick_rally"));
            assert_eq!(ctx.map(|c| c.channel), Some("test".to_owned()));
            Ok("fake_sse_registered".to_owned())
        })
    }

    fn push_outbound(
        &self,
        _channel: String,
        _peer_id: String,
        _message_json: String,
        _ctx: Option<rsclaw_plugin::PluginInvocationContext>,
    ) -> futures::future::BoxFuture<'static, Result<String, String>> {
        Box::pin(async { Ok("fake_push".to_owned()) })
    }

    fn submit_agent_turn(
        &self,
        session_key: String,
        prompt: String,
        _route_json: String,
        ctx: Option<rsclaw_plugin::PluginInvocationContext>,
    ) -> futures::future::BoxFuture<'static, Result<String, String>> {
        self.submit_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            assert_eq!(session_key, "astock:briefing");
            assert!(prompt.contains("ASTOCK_PREFETCHED_JSON"));
            assert!(prompt.contains("600519"));
            assert_eq!(ctx.map(|c| c.channel), Some("test".to_owned()));
            Ok("fake_turn".to_owned())
        })
    }
}

fn astock_plugin_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("RSCLAW_ASTOCK_WASM_PLUGIN_DIR") {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../rsclaw-plugins/astock")
        .components()
        .collect::<PathBuf>();
    local.exists().then_some(local)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn astock_wasm_quote_slash_chart_and_device_headers() {
    let Some(plugin_dir) = astock_plugin_dir() else {
        eprintln!("skipping astock wasm e2e: plugin dir not found");
        return;
    };
    if !plugin_dir.join("astock.wasm").exists() {
        eprintln!("skipping astock wasm e2e: astock.wasm not built");
        return;
    }

    let saw_device_headers = Arc::new(AtomicBool::new(false));
    let background = Arc::new(FakeBackgroundHost::default());
    rsclaw_plugin::set_plugin_background_host(background.clone());
    let base_url = spawn_mock_astock(Arc::clone(&saw_device_headers));

    let mut manifest = rsclaw_plugin::load_manifest(&plugin_dir).expect("load astock manifest");
    manifest.config["baseUrl"] = json!(base_url);
    manifest.config["baseUrlOverride"] = Value::Null;
    manifest.config["apiKey"] = json!("test-key");

    let mut cfg = wasmtime::Config::new();
    cfg.async_support(true);
    let engine = wasmtime::Engine::new(&cfg).expect("wasmtime engine");
    let browser = Arc::new(Mutex::new(None));
    let plugin = rsclaw_plugin::load_wasm_plugin(&manifest, &engine, browser, None, None)
        .await
        .expect("load astock wasm");

    let (tx, mut rx) = broadcast::channel(16);
    let peer_id = format!("peer-astock-e2e-{}", std::process::id());
    let notify_ctx = || rsclaw_plugin::wasm_runtime::WasmNotifyCtx {
        tx: tx.clone(),
        target_id: peer_id.clone(),
        channel: "test".to_owned(),
        agent_id: "main".to_owned(),
        peer_id: peer_id.clone(),
        chat_id: peer_id.clone(),
        session_key: format!("agent:main:test:direct:{peer_id}"),
        is_group: false,
    };

    let slash = plugin
        .call_tool_with_ctx(
            "slash",
            json!({ "text": "/astock quote 600519" }),
            Some(notify_ctx()),
        )
        .await
        .expect("slash quote");
    assert_eq!(slash["data"]["ok"].as_bool(), Some(true));
    assert!(slash["text"].to_string().contains("600519"));

    plugin
        .call_tool_with_ctx(
            "watchlist",
            json!({ "action": "clear" }),
            Some(notify_ctx()),
        )
        .await
        .expect("watchlist clear");
    plugin
        .call_tool_with_ctx(
            "watchlist",
            json!({ "action": "add", "codes": ["600519"] }),
            Some(notify_ctx()),
        )
        .await
        .expect("watchlist add");
    let snapshot = plugin
        .call_tool_with_ctx("snapshot", json!({}), Some(notify_ctx()))
        .await
        .expect("snapshot from watchlist");
    assert_eq!(snapshot["ok"].as_bool(), Some(true));
    assert_eq!(snapshot["used_watchlist"].as_bool(), Some(true));
    assert!(snapshot["markdown"].as_str().unwrap_or_default().contains("600519"));

    let screen = plugin
        .call_tool_with_ctx(
            "slash",
            json!({ "text": "/astock screen quick_rally 5" }),
            Some(notify_ctx()),
        )
        .await
        .expect("slash screen");
    assert_eq!(screen["data"]["ok"].as_bool(), Some(true));
    assert!(screen["text"].as_str().unwrap_or_default().contains("quick_rally"));

    let briefing_list = plugin
        .call_tool_with_ctx(
            "slash",
            json!({ "text": "/astock briefing list" }),
            Some(notify_ctx()),
        )
        .await
        .expect("briefing list");
    assert_eq!(briefing_list["data"]["ok"].as_bool(), Some(true));
    assert!(briefing_list["text"].as_str().unwrap_or_default().contains("premarket"));

    let briefing_next = plugin
        .call_tool_with_ctx(
            "slash",
            json!({ "text": "/astock briefing next" }),
            Some(notify_ctx()),
        )
        .await
        .expect("briefing next");
    assert_eq!(briefing_next["data"]["ok"].as_bool(), Some(true));
    assert!(briefing_next["text"].as_str().unwrap_or_default().contains("下一次"));

    let briefing_run = plugin
        .call_tool_with_ctx(
            "slash",
            json!({ "text": "/astock briefing run premarket" }),
            Some(notify_ctx()),
        )
        .await
        .expect("briefing run");
    assert_eq!(briefing_run["data"]["ok"].as_bool(), Some(true));
    assert_eq!(background.submit_calls.load(Ordering::SeqCst), 1);

    let briefing_register = plugin
        .call_tool_with_ctx(
            "slash",
            json!({ "text": "/astock briefing register" }),
            Some(notify_ctx()),
        )
        .await
        .expect("briefing register");
    assert_eq!(briefing_register["data"]["ok"].as_bool(), Some(true));
    assert_eq!(background.cron_calls.load(Ordering::SeqCst), 1);

    let alerts_register = plugin
        .call_tool_with_ctx(
            "slash",
            json!({ "text": "/astock alerts register" }),
            Some(notify_ctx()),
        )
        .await
        .expect("alerts register");
    assert_eq!(alerts_register["data"]["ok"].as_bool(), Some(true));
    assert_eq!(background.sse_calls.load(Ordering::SeqCst), 1);

    let chart = plugin
        .call_tool_with_ctx(
            "chart",
            json!({ "code": "600519", "period": "1d", "count": 30 }),
            Some(notify_ctx()),
        )
        .await
        .expect("chart");
    assert_eq!(chart["ok"].as_bool(), Some(true));
    let chart_path = chart["path"].as_str().expect("chart path");
    let svg = std::fs::read_to_string(chart_path).expect("read chart svg");
    assert!(svg.contains("<svg"));
    assert!(svg.contains("<polyline"));
    assert!(svg.contains("600519"));

    let outbound = rx.recv().await.expect("chart notification");
    assert_eq!(outbound.channel.as_deref(), Some("test"));
    assert_eq!(outbound.files.len(), 1);
    assert_eq!(outbound.files[0].1, "image/svg+xml");

    assert!(
        saw_device_headers.load(Ordering::SeqCst),
        "mock astock server did not observe device signature headers"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let key_path = rsclaw_config::loader::base_dir()
            .join("device")
            .join("host-ed25519.key");
        let mode = std::fs::metadata(&key_path)
            .expect("device key metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "device key should be owner-only");
    }
}

fn spawn_mock_astock(saw_device_headers: Arc<AtomicBool>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock astock");
    let addr = listener.local_addr().expect("mock addr");
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let saw = Arc::clone(&saw_device_headers);
            thread::spawn(move || handle_mock_conn(stream, saw));
        }
    });
    format!("http://{addr}")
}

fn handle_mock_conn(mut stream: TcpStream, saw_device_headers: Arc<AtomicBool>) {
    let mut buf = [0u8; 8192];
    let Ok(n) = stream.read(&mut buf) else {
        return;
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    if req.to_ascii_lowercase().contains("x-rsclaw-device-signature:")
        && req.to_ascii_lowercase().contains("x-rsclaw-device-public-key:")
    {
        saw_device_headers.store(true, Ordering::SeqCst);
    }
    let path = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let body = if path.starts_with("/v1/quote/600519") {
        json!({
            "code": "600519",
            "market": "SH",
            "price": 1700.0,
            "open": 1680.0,
            "high": 1710.0,
            "low": 1670.0,
            "pre_close": 1660.0,
            "volume": 123456,
            "amount": 210000000.0,
            "timestamp": 1718000000
        })
    } else if path.starts_with("/v1/snapshot") {
        json!([
            {
                "code": "600519",
                "market": "SH",
                "price": 1700.0,
                "pre_close": 1660.0,
                "volume": 123456,
                "amount": 210000000.0,
                "timestamp": 1718000000
            }
        ])
    } else if path.starts_with("/v1/screen/quick/quick_rally") {
        json!({
            "computed_at": "2026-06-16T10:00:00+08:00",
            "cost_credits": 12,
            "count": 1,
            "total_matched": 1,
            "limit": 5,
            "results": [
                { "code": "600519", "name": "贵州茅台", "pct_change": 3.21 }
            ]
        })
    } else if path.starts_with("/v1/kline/600519") {
        json!({
            "code": "600519",
            "period": "1d",
            "adjust": "qfq",
            "klines": (0..30).map(|i| json!({
                "timestamp": 1718000000 + i * 86400,
                "open": 100.0 + i as f64,
                "high": 103.0 + i as f64,
                "low": 98.0 + i as f64,
                "close": 101.0 + i as f64,
                "volume": 1000 + i,
                "amount": 100000.0 + i as f64
            })).collect::<Vec<_>>()
        })
    } else {
        json!({ "ok": true })
    };
    let body = body.to_string();
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
}
