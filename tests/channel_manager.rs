//! Integration tests for `ChannelManager` (concurrent channel limits by tier).

use std::sync::Arc;

use anyhow::Result;
use futures::future::BoxFuture;

use rsclaw::{
    channel::{Channel, ChannelManager, OutboundMessage},
    MemoryTier,
};

// ---------------------------------------------------------------------------
// MockChannel
// ---------------------------------------------------------------------------

struct MockChannel {
    channel_name: String,
}

impl MockChannel {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            channel_name: name.to_owned(),
        })
    }
}

impl Channel for MockChannel {
    fn name(&self) -> &str {
        &self.channel_name
    }

    fn send(&self, _msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Tier limits
// ---------------------------------------------------------------------------

#[test]
fn low_tier_max_3() {
    let mut mgr = ChannelManager::new(MemoryTier::Low);
    assert_eq!(mgr.max_concurrent(), 3);

    for i in 0..3 {
        mgr.register(MockChannel::new(&format!("ch{i}")))
            .expect("should register within limit");
    }
    let err = mgr.register(MockChannel::new("overflow"));
    assert!(
        err.is_err(),
        "Low tier should reject 4th channel registration"
    );
}

#[test]
fn standard_tier_max_8() {
    let mut mgr = ChannelManager::new(MemoryTier::Standard);
    assert_eq!(mgr.max_concurrent(), 8);

    for i in 0..8 {
        mgr.register(MockChannel::new(&format!("ch{i}")))
            .expect("should register within limit");
    }
    let err = mgr.register(MockChannel::new("overflow"));
    assert!(
        err.is_err(),
        "Standard tier should reject 9th channel registration"
    );
}

#[test]
fn high_tier_unlimited() {
    let mut mgr = ChannelManager::new(MemoryTier::High);
    assert_eq!(mgr.max_concurrent(), usize::MAX);

    // Register many channels without hitting a limit.
    for i in 0..100 {
        mgr.register(MockChannel::new(&format!("ch{i}")))
            .expect("High tier should accept many channels");
    }
}

// ---------------------------------------------------------------------------
// get / register
// ---------------------------------------------------------------------------

#[test]
fn get_registered_channel() {
    let mut mgr = ChannelManager::new(MemoryTier::Standard);
    mgr.register(MockChannel::new("telegram")).unwrap();
    mgr.register(MockChannel::new("discord")).unwrap();

    let ch = mgr.get("telegram");
    assert!(ch.is_some(), "should find registered channel");
    assert_eq!(ch.unwrap().name(), "telegram");

    let ch2 = mgr.get("discord");
    assert!(ch2.is_some());
    assert_eq!(ch2.unwrap().name(), "discord");
}

#[test]
fn get_unregistered_returns_none() {
    let mgr = ChannelManager::new(MemoryTier::Standard);
    assert!(
        mgr.get("nonexistent").is_none(),
        "unregistered channel should return None"
    );
}

#[test]
fn register_duplicate_name_overwrites() {
    let mut mgr = ChannelManager::new(MemoryTier::Standard);
    mgr.register(MockChannel::new("telegram")).unwrap();
    // Registering the same name again should overwrite, not increase count.
    mgr.register(MockChannel::new("telegram")).unwrap();

    let ch = mgr.get("telegram");
    assert!(ch.is_some(), "channel should still exist after overwrite");

    // We should still be able to register more channels up to the limit
    // (the duplicate didn't consume an extra slot).
    for i in 1..8 {
        mgr.register(MockChannel::new(&format!("ch{i}")))
            .expect("should register after overwrite");
    }
}
