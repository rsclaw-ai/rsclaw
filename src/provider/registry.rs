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
        // If the model key starts with "<alias_provider>/", strip that prefix
        // before sending to the provider (direct APIs like minimax/openai
        // don't accept "provider/model" format, only aggregators like
        // OpenRouter do). Preserve case of the remaining model id.
        if let Some(alias_provider) = self.model_aliases.get(model) {
            if self.providers.contains_key(alias_provider.as_str()) {
                let prefix = format!("{}/", alias_provider);
                let model_id = model.strip_prefix(&prefix).unwrap_or(model);
                return (alias_provider.as_str(), model_id);
            }
        }
        let (provider, model_id) = Self::parse_model(model);

        // If the inferred provider is registered, use it.
        if self.providers.contains_key(provider) {
            return (provider, model_id);
        }

        // No fallback found, return as-is and let caller handle the error.
        tracing::warn!(
            model,
            inferred_provider = provider,
            "resolve_model: no registered provider or alias fallback found"
        );
        (provider, model_id)
    }
}

fn infer_provider(model: &str) -> &str {
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude") {
        "anthropic"
    } else if m.starts_with("gemini") {
        "gemini"
    } else if m.starts_with("deepseek") {
        "deepseek"
    } else if m.starts_with("qwen") {
        "qwen"
    } else if m.starts_with("glm") || m.starts_with("chatglm") {
        "zhipu"
    } else if m.starts_with("moonshot") || m.starts_with("kimi") {
        "kimi"
    } else if m.starts_with("step") {
        "stepfun"
    } else if m.starts_with("grok") {
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
