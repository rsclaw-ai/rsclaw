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
