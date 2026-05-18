//! Integration test for the FileSource event producer.

use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use rsclaw::gateway::watch::source::{FileSource, SourceImpl};

#[tokio::test]
async fn file_source_emits_appended_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.log");
    tokio::fs::write(&path, "preexisting\n").await.unwrap();

    let (tx, mut rx) = mpsc::channel(64);
    let (stop_tx, stop_rx) = oneshot::channel();
    let src = SourceImpl::File(FileSource { path: path.clone() });

    let handle = tokio::spawn(async move { src.run(tx, stop_rx).await });

    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::fs::write(&path, "preexisting\nhello\nworld\n").await.unwrap();

    let ev1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for first event")
        .expect("channel closed");
    assert_eq!(ev1.event, "line");
    assert_eq!(ev1.raw.as_deref(), Some("hello"));

    let ev2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout for second event")
        .expect("channel closed");
    assert_eq!(ev2.raw.as_deref(), Some("world"));

    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
}

#[tokio::test]
async fn file_source_handles_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rot.log");
    tokio::fs::write(&path, "old1\nold2\n").await.unwrap();

    let (tx, mut rx) = mpsc::channel(64);
    let (stop_tx, stop_rx) = oneshot::channel();
    let src = SourceImpl::File(FileSource { path: path.clone() });
    let handle = tokio::spawn(async move { src.run(tx, stop_rx).await });

    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::fs::write(&path, "fresh\n").await.unwrap();

    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");
    assert_eq!(ev.raw.as_deref(), Some("fresh"));

    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
}
