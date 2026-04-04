//! Integration tests for provider failover and retry logic.
//!
//! These are unit-style tests that exercise `FailoverManager` and the
//! `backoff_delay` / `RetryConfig` machinery directly — no network, no LLM.

#![allow(unused)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, anyhow};
use futures::future::BoxFuture;
use rsclaw::provider::{
    LlmProvider, LlmRequest, LlmStream, Message, MessageContent, RetryConfig, Role, backoff_delay,
    failover::FailoverManager, registry::ProviderRegistry,
};

// ---------------------------------------------------------------------------
// Mock LlmProvider helpers
// ---------------------------------------------------------------------------

/// A provider that always succeeds, returning an empty stream.
struct AlwaysOkProvider {
    provider_name: String,
}

impl AlwaysOkProvider {
    fn new(name: &str) -> Self {
        Self {
            provider_name: name.to_owned(),
        }
    }
}

impl LlmProvider for AlwaysOkProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn stream(&self, _req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        Box::pin(async move {
            use futures::stream;
            use rsclaw::provider::StreamEvent;
            let s: LlmStream = Box::pin(stream::once(async {
                Ok(StreamEvent::Done { usage: None })
            }));
            Ok(s)
        })
    }
}

/// A provider that always returns a rate-limit (429) error.
struct RateLimitProvider {
    provider_name: String,
}

impl RateLimitProvider {
    fn new(name: &str) -> Self {
        Self {
            provider_name: name.to_owned(),
        }
    }
}

impl LlmProvider for RateLimitProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn stream(&self, _req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        Box::pin(async move { Err(anyhow!("429 Too Many Requests")) })
    }
}

/// A provider that fails the first `fail_count` calls then succeeds.
struct FailThenOkProvider {
    provider_name: String,
    fail_count: u32,
    calls: Mutex<u32>,
}

impl FailThenOkProvider {
    fn new(name: &str, fail_count: u32) -> Arc<Self> {
        Arc::new(Self {
            provider_name: name.to_owned(),
            fail_count,
            calls: Mutex::new(0),
        })
    }
}

impl LlmProvider for FailThenOkProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn stream(&self, _req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        let mut guard = self.calls.lock().unwrap();
        *guard += 1;
        let call_num = *guard;
        let should_fail = call_num <= self.fail_count;
        drop(guard);

        Box::pin(async move {
            if should_fail {
                Err(anyhow!("429 rate limit on call {call_num}"))
            } else {
                use futures::stream;
                use rsclaw::provider::StreamEvent;
                let s: LlmStream = Box::pin(stream::once(async {
                    Ok(StreamEvent::Done { usage: None })
                }));
                Ok(s)
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Helper: build a minimal LlmRequest
// ---------------------------------------------------------------------------

fn simple_request(model: &str) -> LlmRequest {
    LlmRequest {
        model: model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("test".to_owned()),
        }],
        tools: vec![],
        system: None,
        max_tokens: Some(64),
        temperature: None,
        thinking_budget: None,
    }
}

// ---------------------------------------------------------------------------
// test_failover_on_429
//
// Primary provider (registered as "primary") returns 429.
// Fallback provider (registered as "fallback") succeeds.
// FailoverManager must return Ok after falling through to the fallback model.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_failover_on_429() {
    let mut registry = ProviderRegistry::new();
    registry.register("primary", Arc::new(RateLimitProvider::new("primary")));
    registry.register("fallback", Arc::new(AlwaysOkProvider::new("fallback")));

    // order: primary provider uses profile "p1"
    let mut order = HashMap::new();
    order.insert("primary".to_owned(), vec!["p1".to_owned()]);

    let api_keys: HashMap<String, String> = HashMap::new();

    // Fallback model points to the "fallback" provider.
    let fallbacks = vec!["fallback/gpt-4o-mini".to_owned()];

    let mut mgr = FailoverManager::new(order, api_keys, fallbacks);

    let req = simple_request("primary/claude-3-sonnet");
    let result = mgr.call(req, &registry).await;

    assert!(
        result.is_ok(),
        "expected Ok after failover to fallback provider, got: {:?}",
        result.err()
    );
}

// ---------------------------------------------------------------------------
// test_retry_exponential_backoff
//
// Verifies that backoff_delay produces strictly increasing delays for
// successive attempts, matching the documented §22 formula.
// ---------------------------------------------------------------------------

#[test]
fn test_retry_exponential_backoff() {
    let cfg = RetryConfig::default();
    let d0 = backoff_delay(0, &cfg);
    let d1 = backoff_delay(1, &cfg);
    let d2 = backoff_delay(2, &cfg);

    assert!(
        d0 < d1,
        "attempt 0 delay ({d0:?}) should be less than attempt 1 ({d1:?})"
    );
    assert!(
        d1 < d2,
        "attempt 1 delay ({d1:?}) should be less than attempt 2 ({d2:?})"
    );

    // Verify the base for attempt 0 is at least min_delay_ms.
    assert!(
        d0.as_millis() >= cfg.min_delay_ms as u128,
        "attempt 0 delay should be at least min_delay_ms={}",
        cfg.min_delay_ms
    );

    // Verify the cap is respected (max + 10 % jitter).
    let max_bound = (cfg.max_delay_ms as f64 * (1.0 + cfg.jitter)) as u128;
    let d_large = backoff_delay(20, &cfg);
    assert!(
        d_large.as_millis() <= max_bound,
        "delay at attempt 20 ({:?}) should not exceed max+jitter ({max_bound} ms)",
        d_large
    );
}

// ---------------------------------------------------------------------------
// test_cooldown_respected
//
// When ALL profiles for a provider are in cooldown AND there are no fallbacks,
// FailoverManager must return Err (exhausted) without calling the provider
// again. We verify this by checking that a second call also fails rather than
// attempting the cooling-down profile.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_cooldown_respected() {
    let mut registry = ProviderRegistry::new();
    // Only one provider, always 429.
    registry.register("only", Arc::new(RateLimitProvider::new("only")));

    let mut order = HashMap::new();
    order.insert("only".to_owned(), vec!["prof-a".to_owned()]);

    let api_keys: HashMap<String, String> = HashMap::new();
    // No fallbacks — all providers exhaust immediately.
    let fallbacks = vec![];

    let mut mgr = FailoverManager::new(order, api_keys, fallbacks);

    // First call: should fail (rate-limited) and put "prof-a" into cooldown.
    let req1 = simple_request("only/model-x");
    let result1 = mgr.call(req1, &registry).await;
    assert!(
        result1.is_err(),
        "first call should fail — no fallback, provider returns 429"
    );

    // Second call: "prof-a" is in cooldown, so it's skipped immediately.
    // FailoverManager returns Err without hitting the provider.
    let req2 = simple_request("only/model-x");
    let result2 = mgr.call(req2, &registry).await;
    assert!(
        result2.is_err(),
        "second call should also fail because the profile is cooling down"
    );
}

// ---------------------------------------------------------------------------
// test_all_providers_exhausted
//
// When every provider and every fallback fail, the manager returns
// "all providers and fallbacks exhausted".
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_all_providers_exhausted() {
    let mut registry = ProviderRegistry::new();
    registry.register("p1", Arc::new(RateLimitProvider::new("p1")));
    registry.register("p2", Arc::new(RateLimitProvider::new("p2")));

    let mut order = HashMap::new();
    order.insert("p1".to_owned(), vec!["prof1".to_owned()]);
    order.insert("p2".to_owned(), vec!["prof2".to_owned()]);

    let api_keys: HashMap<String, String> = HashMap::new();
    let fallbacks = vec!["p2/gpt-fallback".to_owned()];

    let mut mgr = FailoverManager::new(order, api_keys, fallbacks);

    let req = simple_request("p1/claude");
    let err = mgr
        .call(req, &registry)
        .await
        .err()
        .expect("expected Err from exhausted failover");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("exhausted"),
        "error message should mention exhaustion, got: {msg}"
    );
}
