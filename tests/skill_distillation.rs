//! Integration tests for the skill crystallization distillation step.
//!
//! Verifies that `crystallizer::distill_with_llm()` drives a mock
//! `LlmProvider` correctly (accumulates `TextDelta`s, stops at `Done`,
//! errors on empty output) and that `extract_skill_slug()` reads the
//! `name:` field from a SKILL.md frontmatter.

use std::sync::Arc;

use anyhow::Result;
use futures::future::BoxFuture;
use rsclaw::provider::{LlmProvider, LlmRequest, LlmStream, StreamEvent};
use rsclaw::skill::crystallizer::{distill_with_llm, extract_skill_slug};

// ---------------------------------------------------------------------------
// MockProvider — returns a canned SKILL.md as a single TextDelta + Done.
// ---------------------------------------------------------------------------

/// Provider that emits a fixed `canned` text as one `TextDelta` then `Done`.
///
/// If `canned` is empty, no `TextDelta` is emitted (only `Done`), which lets
/// us exercise the `bail!` path in `distill_with_llm`.
struct MockProvider {
    canned: String,
}

impl MockProvider {
    fn new(canned: impl Into<String>) -> Self {
        Self {
            canned: canned.into(),
        }
    }
}

impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn stream(&self, _req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        let canned = self.canned.clone();
        Box::pin(async move {
            use futures::stream;
            let mut events: Vec<Result<StreamEvent>> = Vec::new();
            if !canned.is_empty() {
                events.push(Ok(StreamEvent::TextDelta(canned)));
            }
            events.push(Ok(StreamEvent::Done { usage: None }));
            let s: LlmStream = Box::pin(stream::iter(events));
            Ok(s)
        })
    }
}

// ---------------------------------------------------------------------------
// Sample SKILL.md — used by the distill + extract_slug tests.
// ---------------------------------------------------------------------------

const SAMPLE_SKILL: &str = "---
name: web-search-helper
description: >
  How to run a web search and merge the top results.
---
# Web Search Helper

1. Build the query.
2. Fetch results.
3. Merge and return.
";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn distill_with_llm_returns_provider_text() {
    let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(SAMPLE_SKILL));

    let result = distill_with_llm("ignored prompt", provider, "mock-model".to_owned())
        .await
        .expect("distill should succeed when provider yields text");

    assert_eq!(
        result, SAMPLE_SKILL,
        "distilled output should equal the provider's TextDelta"
    );
}

#[tokio::test]
async fn extract_skill_slug_from_distilled_output() {
    let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(SAMPLE_SKILL));

    let distilled = distill_with_llm("ignored prompt", provider, "mock-model".to_owned())
        .await
        .expect("distill should succeed");

    let slug = extract_skill_slug(&distilled, "fallback-name");
    assert_eq!(
        slug, "web-search-helper",
        "slug should be parsed from the frontmatter name: field"
    );
}

#[tokio::test]
async fn distill_with_llm_errors_on_empty_output() {
    let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(""));

    let err = distill_with_llm("ignored prompt", provider, "mock-model".to_owned())
        .await
        .err()
        .expect("distill should fail when provider yields no text");

    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("empty"),
        "error message should mention empty output, got: {msg}"
    );
}
