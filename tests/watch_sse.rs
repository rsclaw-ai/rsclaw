//! Integration test for SseSource — single-connection happy path.

use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use rsclaw::gateway::watch::source::{SourceImpl, SseSource};

/// Boot a minimal HTTP/1.1 server that emits 3 SSE events then waits for stop.
async fn boot_sse_server() -> (String, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/events");
    let (kill_tx, mut kill_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut kill_rx => break,
                accept = listener.accept() => {
                    let (mut sock, _) = match accept { Ok(s) => s, Err(_) => break };
                    tokio::spawn(async move {
                        // Read & discard the request — we don't care about details.
                        let mut buf = [0u8; 1024];
                        let _ = tokio::time::timeout(Duration::from_millis(200),
                            tokio::io::AsyncReadExt::read(&mut sock, &mut buf)).await;
                        let header =
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
                        let _ = sock.write_all(header.as_bytes()).await;
                        let _ = sock.write_all(b"event: hit\ndata: {\"code\":\"600519\"}\n\n").await;
                        let _ = sock.write_all(b"data: {\"x\":1}\n\n").await;
                        let _ = sock.write_all(b"id: 42\ndata: {\"x\":2}\n\n").await;
                        // Hold the conn open briefly so the client has time to read.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    });
                }
            }
        }
    });

    (url, kill_tx)
}

#[tokio::test]
async fn sse_source_reads_three_events() {
    let (url, kill) = boot_sse_server().await;

    let (tx, mut rx) = mpsc::channel(16);
    let (stop_tx, stop_rx) = oneshot::channel();
    let src = SourceImpl::Sse(SseSource { url, headers: vec![] });
    let handle = tokio::spawn(async move { src.run(tx, stop_rx).await });

    let e1 = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert_eq!(e1.event, "hit");
    let e2 = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert_eq!(e2.event, "message");
    let e3 = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert_eq!(e3.event_id.as_deref(), Some("42"));

    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    let _ = kill.send(());
}

async fn boot_flaky_sse_server() -> (String, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/events");
    let (kill_tx, mut kill_rx) = oneshot::channel();
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut kill_rx => break,
                accept = listener.accept() => {
                    let (mut sock, _) = match accept { Ok(s) => s, Err(_) => break };
                    let attempts = attempts.clone();
                    tokio::spawn(async move {
                        let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let mut buf = [0u8; 1024];
                        let _ = tokio::time::timeout(Duration::from_millis(200),
                            tokio::io::AsyncReadExt::read(&mut sock, &mut buf)).await;
                        let header =
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
                        let _ = sock.write_all(header.as_bytes()).await;
                        if n == 0 {
                            // First attempt: send retry: 200 + one event, then close abruptly.
                            let _ = sock.write_all(b"retry: 200\ndata: {\"a\":1}\n\n").await;
                            // Close socket.
                        } else {
                            // Second attempt: send a second event then linger.
                            let _ = sock.write_all(b"data: {\"a\":2}\n\n").await;
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    });
                }
            }
        }
    });
    (url, kill_tx)
}

#[tokio::test]
async fn sse_source_reconnects_after_disconnect() {
    let (url, kill) = boot_flaky_sse_server().await;
    let (tx, mut rx) = mpsc::channel(16);
    let (stop_tx, stop_rx) = oneshot::channel();
    let src = SourceImpl::Sse(SseSource { url, headers: vec![] });
    let handle = tokio::spawn(async move { src.run(tx, stop_rx).await });

    // Drain past lifecycle events (`_disconnect` is emitted between the
    // first connection ending and the reconnect starting).
    let mut got: Vec<serde_json::Value> = Vec::new();
    while got.len() < 2 {
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if !ev.event.starts_with('_') {
            got.push(ev.data);
        }
    }
    assert_eq!(got[0], serde_json::json!({"a": 1}));
    assert_eq!(got[1], serde_json::json!({"a": 2}));

    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    let _ = kill.send(());
}

#[tokio::test]
async fn sse_source_terminates_on_403() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/events");
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = tokio::time::timeout(Duration::from_millis(200),
                tokio::io::AsyncReadExt::read(&mut sock, &mut buf)).await;
            let _ = sock.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await;
        }
    });

    let (tx, mut rx) = mpsc::channel(16);
    let (_stop_tx, stop_rx) = oneshot::channel();
    let src = SourceImpl::Sse(SseSource { url, headers: vec![] });
    let handle = tokio::spawn(async move { src.run(tx, stop_rx).await });

    let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
    assert_eq!(ev.event, "_error");
    assert_eq!(ev.data["fatal"], serde_json::Value::Bool(true));
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

// Heartbeat-timeout (90s no-byte) is tested manually; CI skips it to
// keep test runtime under 1 minute. To exercise: boot a server that
// sends the response headers but no body, then watch for `_timeout` on rx.

#[tokio::test]
async fn sse_source_sends_last_event_id_on_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/events");
    let saw_resume_header = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let saw_clone = saw_resume_header.clone();
    let attempts_clone = attempts.clone();
    tokio::spawn(async move {
        for _ in 0..4 {
            if let Ok((mut sock, _)) = listener.accept().await {
                let n = attempts_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                if let Ok(Ok(read)) = tokio::time::timeout(
                    Duration::from_millis(500),
                    tokio::io::AsyncReadExt::read(&mut sock, &mut buf),
                )
                .await
                {
                    let req = String::from_utf8_lossy(&buf[..read]);
                    if req.to_lowercase().contains("last-event-id: 99") {
                        saw_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                let header =
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(header.as_bytes()).await;
                if n == 0 {
                    let _ = sock.write_all(b"id: 99\ndata: {\"x\":1}\nretry: 200\n\n").await;
                    // Close abruptly to force reconnect.
                } else {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    });

    let (tx, mut rx) = mpsc::channel(16);
    let (stop_tx, stop_rx) = oneshot::channel();
    let src = SourceImpl::Sse(SseSource { url, headers: vec![] });
    let handle = tokio::spawn(async move { src.run(tx, stop_rx).await });

    // Receive the first event and wait long enough for the reconnect attempt.
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(saw_resume_header.load(std::sync::atomic::Ordering::SeqCst));

    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}
