//! Integration tests for ProviderRegistry: resolve_model, parse_model,
//! alias priority, fallback chains, and CRUD operations.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use futures::future::BoxFuture;
use rsclaw::provider::{LlmProvider, LlmRequest, LlmStream, registry::ProviderRegistry};

// ---------------------------------------------------------------------------
// Minimal mock provider
// ---------------------------------------------------------------------------

struct DummyProvider(String);

impl LlmProvider for DummyProvider {
    fn name(&self) -> &str {
        &self.0
    }

    fn stream(&self, _: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        Box::pin(async { Err(anyhow::anyhow!("dummy")) })
    }
}

fn dummy(name: &str) -> Arc<dyn LlmProvider> {
    Arc::new(DummyProvider(name.to_owned()))
}

// ---------------------------------------------------------------------------
// parse_model tests
// ---------------------------------------------------------------------------

#[test]
fn parse_model_explicit_prefix() {
    assert_eq!(
        ProviderRegistry::parse_model("anthropic/claude-sonnet-4-5"),
        ("anthropic", "claude-sonnet-4-5")
    );
    assert_eq!(
        ProviderRegistry::parse_model("openai/gpt-4o"),
        ("openai", "gpt-4o")
    );
}

#[test]
fn parse_model_inferred_anthropic() {
    assert_eq!(
        ProviderRegistry::parse_model("claude-3-5-sonnet-20241022"),
        ("anthropic", "claude-3-5-sonnet-20241022")
    );
}

#[test]
fn parse_model_inferred_gemini() {
    assert_eq!(
        ProviderRegistry::parse_model("gemini-2.0-flash"),
        ("gemini", "gemini-2.0-flash")
    );
}

#[test]
fn parse_model_inferred_deepseek() {
    assert_eq!(
        ProviderRegistry::parse_model("deepseek-chat"),
        ("deepseek", "deepseek-chat")
    );
}

#[test]
fn parse_model_inferred_qwen() {
    assert_eq!(
        ProviderRegistry::parse_model("qwen-turbo"),
        ("qwen", "qwen-turbo")
    );
}

#[test]
fn parse_model_inferred_zhipu() {
    assert_eq!(ProviderRegistry::parse_model("glm-4"), ("zhipu", "glm-4"));
    assert_eq!(
        ProviderRegistry::parse_model("chatglm-pro"),
        ("zhipu", "chatglm-pro")
    );
}

#[test]
fn parse_model_inferred_kimi() {
    assert_eq!(
        ProviderRegistry::parse_model("moonshot-v1-8k"),
        ("kimi", "moonshot-v1-8k")
    );
    assert_eq!(
        ProviderRegistry::parse_model("kimi-2"),
        ("kimi", "kimi-2")
    );
}

#[test]
fn parse_model_inferred_stepfun() {
    assert_eq!(
        ProviderRegistry::parse_model("step-1-8k"),
        ("stepfun", "step-1-8k")
    );
}

#[test]
fn parse_model_inferred_xai() {
    assert_eq!(ProviderRegistry::parse_model("grok-2"), ("xai", "grok-2"));
}

#[test]
fn parse_model_inferred_openai_fallback() {
    // Unknown models default to openai
    assert_eq!(
        ProviderRegistry::parse_model("gpt-4o"),
        ("openai", "gpt-4o")
    );
    assert_eq!(
        ProviderRegistry::parse_model("o1-preview"),
        ("openai", "o1-preview")
    );
    assert_eq!(
        ProviderRegistry::parse_model("some-unknown-model"),
        ("openai", "some-unknown-model")
    );
}

#[test]
fn parse_model_with_multiple_slashes() {
    // Only splits on first slash
    let (provider, model_id) = ProviderRegistry::parse_model("provider/org/model-name");
    assert_eq!(provider, "provider");
    assert_eq!(model_id, "org/model-name");
}

// ---------------------------------------------------------------------------
// resolve_model tests
// ---------------------------------------------------------------------------

#[test]
fn resolve_model_explicit_prefix_registered() {
    let mut reg = ProviderRegistry::new();
    reg.register("anthropic", dummy("anthropic"));
    let (p, m) = reg.resolve_model("anthropic/claude-sonnet-4-5");
    assert_eq!(p, "anthropic");
    assert_eq!(m, "claude-sonnet-4-5");
}

#[test]
fn resolve_model_inferred_registered() {
    let mut reg = ProviderRegistry::new();
    reg.register("anthropic", dummy("anthropic"));
    let (p, m) = reg.resolve_model("claude-3-5-sonnet-20241022");
    assert_eq!(p, "anthropic");
    assert_eq!(m, "claude-3-5-sonnet-20241022");
}

#[test]
fn resolve_model_alias_priority_over_inference() {
    let mut reg = ProviderRegistry::new();
    reg.register("gaterouter", dummy("gaterouter"));
    reg.register("anthropic", dummy("anthropic"));

    let mut aliases = HashMap::new();
    // Alias routes the full model key to "gaterouter"
    aliases.insert("claude-sonnet-4-5".to_owned(), "gaterouter".to_owned());
    reg.set_model_aliases(aliases);

    let (p, m) = reg.resolve_model("claude-sonnet-4-5");
    // Alias should win over inference to "anthropic"
    assert_eq!(p, "gaterouter");
    assert_eq!(m, "claude-sonnet-4-5");
}

#[test]
fn resolve_model_alias_target_not_registered_falls_through() {
    let mut reg = ProviderRegistry::new();
    reg.register("anthropic", dummy("anthropic"));

    let mut aliases = HashMap::new();
    // Alias points to "missing_provider" which is not registered
    aliases.insert("claude-sonnet-4-5".to_owned(), "missing_provider".to_owned());
    reg.set_model_aliases(aliases);

    let (p, m) = reg.resolve_model("claude-sonnet-4-5");
    // Should fall through alias and use inference -> anthropic
    assert_eq!(p, "anthropic");
    assert_eq!(m, "claude-sonnet-4-5");
}

#[test]
fn resolve_model_fallback_to_custom() {
    let mut reg = ProviderRegistry::new();
    reg.register("custom", dummy("custom"));
    // "deepseek" not registered, should fallback to "custom"
    let (p, m) = reg.resolve_model("deepseek-chat");
    assert_eq!(p, "custom");
    assert_eq!(m, "deepseek-chat");
}

#[test]
fn resolve_model_fallback_to_ollama() {
    let mut reg = ProviderRegistry::new();
    reg.register("ollama", dummy("ollama"));
    // "deepseek" not registered, no "custom", should fallback to "ollama"
    let (p, m) = reg.resolve_model("deepseek-chat");
    assert_eq!(p, "ollama");
    assert_eq!(m, "deepseek-chat");
}

#[test]
fn resolve_model_fallback_to_first_registered() {
    let mut reg = ProviderRegistry::new();
    reg.register("myhost", dummy("myhost"));
    // No custom, no ollama -- should fallback to whatever is registered
    let (p, m) = reg.resolve_model("deepseek-chat");
    assert_eq!(p, "myhost");
    assert_eq!(m, "deepseek-chat");
}

#[test]
fn resolve_model_no_providers_returns_inferred() {
    let reg = ProviderRegistry::new();
    let (p, m) = reg.resolve_model("claude-3-5-sonnet");
    // No providers registered; returns the inferred pair
    assert_eq!(p, "anthropic");
    assert_eq!(m, "claude-3-5-sonnet");
}

#[test]
fn resolve_model_custom_beats_ollama() {
    let mut reg = ProviderRegistry::new();
    reg.register("custom", dummy("custom"));
    reg.register("ollama", dummy("ollama"));
    let (p, _) = reg.resolve_model("deepseek-chat");
    assert_eq!(p, "custom");
}

// ---------------------------------------------------------------------------
// Registry CRUD
// ---------------------------------------------------------------------------

#[test]
fn registry_register_and_get() {
    let mut reg = ProviderRegistry::new();
    reg.register("test", dummy("test"));
    assert!(reg.get("test").is_ok());
    assert_eq!(reg.get("test").unwrap().name(), "test");
}

#[test]
fn registry_get_missing_returns_error() {
    let reg = ProviderRegistry::new();
    assert!(reg.get("nonexistent").is_err());
}

#[test]
fn registry_names() {
    let mut reg = ProviderRegistry::new();
    reg.register("alpha", dummy("alpha"));
    reg.register("beta", dummy("beta"));
    let mut names = reg.names();
    names.sort();
    assert_eq!(names, vec!["alpha", "beta"]);
}

#[test]
fn registry_register_overwrites() {
    let mut reg = ProviderRegistry::new();
    reg.register("p", dummy("old"));
    reg.register("p", dummy("new"));
    assert_eq!(reg.get("p").unwrap().name(), "new");
}
