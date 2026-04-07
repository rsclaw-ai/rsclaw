//! Provider registry: maps provider name → `Arc<dyn LlmProvider>`.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;

use super::LlmProvider;

#[derive(Default)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    /// Model alias table: full model key -> target provider name.
    /// When a model matches, the request is routed to the alias provider
    /// with the original full model key preserved as model_id.
    model_aliases: HashMap<String, String>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider under a given name.
    pub fn register(&mut self, name: impl Into<String>, provider: Arc<dyn LlmProvider>) {
        self.providers.insert(name.into(), provider);
    }

    /// Set model aliases from config (agents.defaults.models).
    /// Maps a full model key (e.g. "minimax/minimax-m2.1") to a provider name
    /// (e.g. "gaterouter"). When resolved, the full model key is preserved as
    /// model_id so the target provider receives it intact.
    pub fn set_model_aliases(&mut self, aliases: HashMap<String, String>) {
        self.model_aliases = aliases;
    }

    /// Look up a provider by name.
    pub fn get(&self, name: &str) -> Result<Arc<dyn LlmProvider>> {
        self.providers
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("provider not registered: {name}"))
    }

    /// List all registered provider names.
    pub fn names(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }

    /// Parse a model string like `"anthropic/claude-sonnet-4-5"` into
    /// `(provider_name, model_id)`.
    ///
    /// When no explicit `provider/` prefix is present, the provider is
    /// inferred from well-known model-name prefixes.
    pub fn parse_model(model: &str) -> (&str, &str) {
        if let Some((provider, model_id)) = model.split_once('/') {
            (provider, model_id)
        } else {
            (infer_provider(model), model)
        }
    }

    /// Resolve a model string against this registry. Like `parse_model` but
    /// first checks model aliases (agents.defaults.models), then if the
    /// inferred provider is not registered, falls back to a registered
    /// provider (preferring custom > ollama > first).
    pub fn resolve_model<'a>(&'a self, model: &'a str) -> (&'a str, &'a str) {
        // Check model alias table first: full model key -> provider name.
        // Preserve the original model key as model_id so the target provider
        // receives it intact (e.g. "minimax/minimax-m2.1" stays as-is).
        if let Some(alias_provider) = self.model_aliases.get(model) {
            if self.providers.contains_key(alias_provider.as_str()) {
                return (alias_provider.as_str(), model);
            }
        }
        let (provider, model_id) = Self::parse_model(model);
        if self.providers.contains_key(provider) {
            return (provider, model_id);
        }
        // Provider not registered -- pick the best fallback.
        if self.providers.contains_key("custom") {
            return ("custom", model_id);
        }
        if self.providers.contains_key("ollama") {
            return ("ollama", model_id);
        }
        // Use first registered provider if there is one.
        if let Some(name) = self.providers.keys().next() {
            return (name.as_str(), model_id);
        }
        (provider, model_id)
    }
}

/// Default API base URL for a provider name. Returns `(base_url, auth_style)`.
///
/// `auth_style` is one of: `"bearer"`, `"x-api-key"`, `"none"`.
///
/// This is the **single source of truth** for provider URLs. Used by:
///   - `gateway/startup.rs` (runtime provider construction)
///   - `server/mod.rs` (UI test-provider and list-models endpoints)
pub fn provider_base_url(provider: &str) -> (&'static str, &'static str) {
    match provider {
        "anthropic"   => ("https://api.anthropic.com",                           "x-api-key"),
        "openai"      => ("https://api.openai.com/v1",                           "bearer"),
        "deepseek"    => ("https://api.deepseek.com/v1",                         "bearer"),
        "qwen"        => ("https://dashscope.aliyuncs.com/compatible-mode/v1",   "bearer"),
        "minimax"     => ("https://api.minimax.chat/v1",                         "bearer"),
        "kimi"
        | "moonshot"  => ("https://api.moonshot.cn/v1",                          "bearer"),
        "zhipu"       => ("https://open.bigmodel.cn/api/paas/v4",               "bearer"),
        "groq"        => ("https://api.groq.com/openai/v1",                      "bearer"),
        "grok"
        | "xai"       => ("https://api.x.ai/v1",                                "bearer"),
        "gemini"      => ("https://generativelanguage.googleapis.com/v1beta",    "bearer"),
        "siliconflow" => ("https://api.siliconflow.cn/v1",                       "bearer"),
        "openrouter"  => ("https://openrouter.ai/api/v1",                        "bearer"),
        "gaterouter"  => ("https://api.gaterouter.com/v1",                       "bearer"),
        "stepfun"     => ("https://api.stepfun.com/v1",                          "bearer"),
        "ollama"      => ("http://localhost:11434",                               "none"),
        _             => ("",                                                     "bearer"),
    }
}

fn infer_provider(model: &str) -> &str {
    if model.starts_with("claude") {
        "anthropic"
    } else if model.starts_with("gemini") {
        "gemini"
    } else if model.starts_with("deepseek") {
        "deepseek"
    } else if model.starts_with("qwen") {
        "qwen"
    } else if model.starts_with("glm") || model.starts_with("chatglm") {
        "zhipu"
    } else if model.starts_with("moonshot") || model.starts_with("kimi") {
        "kimi"
    } else if model.starts_with("step") {
        "stepfun"
    } else if model.starts_with("grok") {
        "xai"
    } else {
        "openai"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_model_explicit_prefix() {
        assert_eq!(
            ProviderRegistry::parse_model("anthropic/claude-sonnet-4-5"),
            ("anthropic", "claude-sonnet-4-5")
        );
        assert_eq!(
            ProviderRegistry::parse_model("groq/llama-3-70b"),
            ("groq", "llama-3-70b")
        );
        assert_eq!(
            ProviderRegistry::parse_model("gemini/gemini-2.0-flash"),
            ("gemini", "gemini-2.0-flash")
        );
    }

    #[test]
    fn parse_model_inferred_anthropic() {
        assert_eq!(
            ProviderRegistry::parse_model("claude-sonnet-4-5"),
            ("anthropic", "claude-sonnet-4-5")
        );
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
    fn parse_model_inferred_openai() {
        assert_eq!(
            ProviderRegistry::parse_model("gpt-4o"),
            ("openai", "gpt-4o")
        );
        assert_eq!(
            ProviderRegistry::parse_model("o1-preview"),
            ("openai", "o1-preview")
        );
        assert_eq!(
            ProviderRegistry::parse_model("o3-mini"),
            ("openai", "o3-mini")
        );
    }

    #[test]
    fn parse_model_unknown_defaults_to_openai() {
        assert_eq!(
            ProviderRegistry::parse_model("some-unknown-model"),
            ("openai", "some-unknown-model")
        );
    }

    #[test]
    fn parse_model_chinese_providers() {
        assert_eq!(
            ProviderRegistry::parse_model("deepseek-chat"),
            ("deepseek", "deepseek-chat")
        );
        assert_eq!(
            ProviderRegistry::parse_model("qwen-turbo"),
            ("qwen", "qwen-turbo")
        );
        assert_eq!(ProviderRegistry::parse_model("glm-4"), ("zhipu", "glm-4"));
        assert_eq!(
            ProviderRegistry::parse_model("moonshot-v1-8k"),
            ("kimi", "moonshot-v1-8k")
        );
        assert_eq!(ProviderRegistry::parse_model("grok-2"), ("xai", "grok-2"));
    }
}
