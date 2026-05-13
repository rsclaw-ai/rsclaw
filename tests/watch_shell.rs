//! Integration test for the ShellSource event producer.

use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use rsclaw::gateway::watch::source::{ShellSource, SourceImpl};

#[tokio::test]
async fn shell_source_streams_stdout_lines() {
    let cmd = if cfg!(target_os = "windows") {
        "echo a; echo b; echo c; Start-Sleep -Seconds 5".to_owned()
    } else {
        "echo a; echo b; echo c; sleep 5".to_owned()
    };

    let (tx, mut rx) = mpsc::channel(64);
    let (stop_tx, stop_rx) = oneshot::channel();
    let src = SourceImpl::Shell(ShellSource { cmd });
    let handle = tokio::spawn(async move { src.run(tx, stop_rx).await });

    let mut got: Vec<String> = Vec::new();
    for _ in 0..3 {
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        got.push(ev.raw.unwrap_or_default());
    }
    assert_eq!(got, vec!["a", "b", "c"]);

    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn shell_source_emits_disconnect_on_exit() {
    let cmd = "echo done".to_owned();
    let (tx, mut rx) = mpsc::channel(64);
    let (_stop_tx, stop_rx) = oneshot::channel();
    let src = SourceImpl::Shell(ShellSource { cmd });
    let handle = tokio::spawn(async move { src.run(tx, stop_rx).await });

    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.raw.as_deref(), Some("done"));
    let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.event, "_disconnect");

    let _ = handle.await;
}
