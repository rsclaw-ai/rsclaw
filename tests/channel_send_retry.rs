//! Integration tests for `send_with_retry` retry logic.

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use anyhow::Result;
use futures::future::BoxFuture;

use rsclaw::{
    channel::{send_with_retry, Channel, OutboundMessage},
    provider::RetryConfig,
};

// ---------------------------------------------------------------------------
// CountingChannel
// ---------------------------------------------------------------------------

/// A test channel that fails the first `fail_first_n` sends, then succeeds.
struct CountingChannel {
    channel_name: String,
    call_count: AtomicU32,
    fail_first_n: u32,
}

impl CountingChannel {
    fn new(fail_first_n: u32) -> Arc<Self> {
        Arc::new(Self {
            channel_name: "counting".to_owned(),
            call_count: AtomicU32::new(0),
            fail_first_n,
        })
    }

    fn calls(&self) -> u32 {
        self.call_count.load(Ordering::SeqCst)
    }
}

impl Channel for CountingChannel {
    fn name(&self) -> &str {
        &self.channel_name
    }

    fn send(&self, _msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if n < self.fail_first_n {
                anyhow::bail!("transient failure #{n}");
            }
            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn test_msg() -> OutboundMessage {
    OutboundMessage {
        target_id: "test".to_owned(),
        is_group: false,
        text: "hello".to_owned(),
        reply_to: None,
        images: vec![],
        ..Default::default()
    }
}

fn fast_retry(attempts: u32) -> RetryConfig {
    RetryConfig {
        attempts,
        min_delay_ms: 1, // tiny delays for tests
        max_delay_ms: 5,
        jitter: 0.0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_succeeds_on_second_attempt() {
    let ch = CountingChannel::new(1); // fail first, then succeed
    let config = fast_retry(3);
    let result = send_with_retry(ch.as_ref(), test_msg(), &config).await;
    assert!(result.is_ok(), "should succeed on second attempt");
    assert_eq!(ch.calls(), 2, "should have been called exactly twice");
}

#[tokio::test]
async fn retry_exhausted_returns_last_error() {
    let ch = CountingChannel::new(100); // always fail
    let config = fast_retry(3);
    let result = send_with_retry(ch.as_ref(), test_msg(), &config).await;
    assert!(result.is_err(), "should fail after all retries exhausted");
    assert_eq!(ch.calls(), 3, "should have been called 3 times");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("transient failure"),
        "error should be from the channel: {err_msg}"
    );
}

#[tokio::test]
async fn retry_first_attempt_succeeds() {
    let ch = CountingChannel::new(0); // never fail
    let config = fast_retry(3);
    let result = send_with_retry(ch.as_ref(), test_msg(), &config).await;
    assert!(result.is_ok(), "should succeed on first attempt");
    assert_eq!(ch.calls(), 1, "should have been called exactly once");
}

#[tokio::test]
async fn retry_with_single_attempt() {
    let ch = CountingChannel::new(1); // fail first (and only) attempt
    let config = fast_retry(1);
    let result = send_with_retry(ch.as_ref(), test_msg(), &config).await;
    assert!(
        result.is_err(),
        "single attempt that fails should return Err"
    );
    assert_eq!(ch.calls(), 1, "should have been called exactly once");
}
