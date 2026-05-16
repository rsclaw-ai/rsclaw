//! Provider registry construction.
//!
//! Builds the [`ProviderRegistry`] from config and environment variables.

use std::{collections::HashMap, sync::Arc};

use tracing::info;

use crate::{
    config::runtime::RuntimeConfig,
    provider::{
        LlmProvider,
        anthropic::{self as anthropic, AnthropicProvider},
        gemini::{self as gemini, GeminiProvider},
        openai::OpenAiProvider,
        registry::ProviderRegistry,
        rsclaw::RsclawProvider,
    },
};

pub(crate) fn build_providers(config: &RuntimeConfig) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();

    if let Some(models_cfg) = &config.model.models {
        for (name, provider_cfg) in &models_cfg.providers {
            let api_key = provider_cfg
                .api_key
                .as_ref()
                .and_then(|k| k.as_plain().map(str::to_owned))
                .or_else(|| std::env::var(format!("{}_API_KEY", name.to_uppercase())).ok());

            let base_url = provider_cfg.base_url.clone().or_else(|| {
                // Fall back to well-known base URLs for named providers.
                match name.as_str() {
                    "qwen" => Some("https://dashscope.aliyuncs.com/compatible-mode/v1".to_owned()),
                    "deepseek" => Some("https://api.deepseek.com/v1".to_owned()),
                    "kimi" | "moonshot" => Some("https://api.moonshot.cn/v1".to_owned()),
                    "zhipu" => Some("https://open.bigmodel.cn/api/paas/v4".to_owned()),
                    "minimax" => Some("https://api.minimaxi.com/v1".to_owned()),
                    "siliconflow" => Some("https://api.siliconflow.cn/v1".to_owned()),
                    "groq"        => Some("https://api.groq.com/openai/v1".to_owned()),
                    "openrouter"  => Some("https://openrouter.ai/api/v1".to_owned()),
                    "gaterouter"  => Some("https://api.gaterouter.ai/openai/v1".to_owned()),
                    "grok" | "xai" => Some("https://api.x.ai/v1".to_owned()),
                    _ => None,
                }
            });

            // Resolve User-Agent: provider > gateway > built-in default.
            let user_agent = provider_cfg
                .user_agent
                .clone()
                .or_else(|| config.gateway.user_agent.clone());

            // Determine API format: explicit `api` field > name-based inference.
            let api_format = provider_cfg.api.clone().unwrap_or_else(|| {
                use crate::config::schema::ApiFormat;
                match name.as_str() {
                    "anthropic" => ApiFormat::Anthropic,
                    "gemini" => ApiFormat::Gemini,
                    "doubao" | "bytedance" => ApiFormat::OpenAiResponses,
                    "ollama" => ApiFormat::Ollama,
                    // Bare `rsclaw` name implies the stateful session
                    // protocol — same shortcut as `anthropic`/`gemini`
                    // above. Users who name their provider differently
                    // (e.g. a self-hosted worker called `local-llm`)
                    // must set `api: "rsclaw"` explicitly.
                    "rsclaw" => ApiFormat::Rsclaw,
                    _ => ApiFormat::OpenAiCompletions,
                }
            });

            let provider: Arc<dyn LlmProvider> = match (name.as_str(), &api_format) {
                ("anthropic", _)
                | (_, &crate::config::schema::ApiFormat::Anthropic)
                | (_, &crate::config::schema::ApiFormat::AnthropicMessages) => {
                    let key = api_key
                        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                        .unwrap_or_default();
                    let url = base_url.unwrap_or_else(|| anthropic::ANTHROPIC_API_BASE.to_owned());
                    Arc::new(AnthropicProvider::with_user_agent(key, url, user_agent))
                }
                ("gemini", _) => {
                    let key = api_key
                        .or_else(|| std::env::var("GEMINI_API_KEY").ok())
                        .unwrap_or_default();
                    let url = base_url.unwrap_or_else(|| gemini::GEMINI_API_BASE.to_owned());
                    Arc::new(GeminiProvider::with_user_agent(key, url, user_agent))
                }
                (_, &crate::config::schema::ApiFormat::Ollama) => {
                    // Ollama backend: reasoning models use native /api/chat
                    let key = api_key.or_else(|| std::env::var("OPENAI_API_KEY").ok());
                    let url = base_url.unwrap_or_else(|| "http://localhost:11434".to_owned());
                    Arc::new(OpenAiProvider::ollama_with_ua(url, key, user_agent))
                }
                (_, &crate::config::schema::ApiFormat::OpenAiResponses) => {
                    let key = api_key.or_else(|| std::env::var("OPENAI_API_KEY").ok());
                    let url = base_url
                        .unwrap_or_else(|| crate::provider::openai::OPENAI_API_BASE.to_owned());
                    Arc::new(OpenAiProvider::responses_with_ua(url, key, user_agent))
                }
                (_, &crate::config::schema::ApiFormat::Rsclaw) => {
                    // rsclaw stateful session protocol (kvCacheMode=2).
                    // `baseUrl` should point at either rsclaw-server
                    // (e.g. `https://api.rsclaw.ai/v1/agent`) or a
                    // direct rsclaw-llm worker (e.g.
                    // `http://localhost:9999`). `apiKey` is the bearer
                    // token; falls back to RSCLAW_KEY env, then to
                    // None (acceptable when targeting an unauth'd
                    // local worker).
                    let key = api_key
                        .or_else(|| std::env::var("RSCLAW_KEY").ok())
                        .or_else(|| std::env::var("RSCLAW_SERVER_KEY").ok())
                        .filter(|s| !s.is_empty());
                    let url = base_url.unwrap_or_else(|| {
                        crate::provider::rsclaw::RSCLAW_DEFAULT_BASE.to_owned()
                    });
                    let provider = crate::provider::rsclaw::RsclawProvider::new(url, key);
                    let provider = match provider_cfg.prefix_id.clone() {
                        Some(pid) => provider.with_prefix_id(pid),
                        None => provider,
                    };
                    Arc::new(provider)
                }
                _ => {
                    // OpenAI-compatible (covers openai-completions,
                    // llama.cpp, vLLM, SGLang, etc.)
                    let key = api_key.or_else(|| std::env::var("OPENAI_API_KEY").ok());
                    if let Some(url) = base_url {
                        Arc::new(OpenAiProvider::with_user_agent(url, key, user_agent))
                    } else {
                        Arc::new(OpenAiProvider::with_user_agent(
                            crate::provider::openai::OPENAI_API_BASE,
                            key,
                            user_agent,
                        ))
                    }
                }
            };

            tracing::info!(name=%name, api=?api_format, "provider registered");
            registry.register(name.clone(), provider);
        }
    }

    // Auto-register from environment variables.
    if !registry.names().contains(&"anthropic")
        && let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
    {
        registry.register("anthropic", Arc::new(AnthropicProvider::new(key)));
    }
    if !registry.names().contains(&"openai")
        && let Ok(key) = std::env::var("OPENAI_API_KEY")
    {
        registry.register("openai", Arc::new(OpenAiProvider::new(key)));
    }
    if !registry.names().contains(&"gemini")
        && let Ok(key) = std::env::var("GEMINI_API_KEY")
    {
        registry.register("gemini", Arc::new(GeminiProvider::new(key)));
    }

    // Auto-register OpenAI-compatible providers from env vars.
    let compat_providers = [
        // --- International ---
        ("groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
        (
            "deepseek",
            "https://api.deepseek.com/v1",
            "DEEPSEEK_API_KEY",
        ),
        ("mistral", "https://api.mistral.ai/v1", "MISTRAL_API_KEY"),
        (
            "together",
            "https://api.together.xyz/v1",
            "TOGETHER_API_KEY",
        ),
        (
            "openrouter",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
        ),
        ("xai", "https://api.x.ai/v1", "XAI_API_KEY"),
        ("cerebras", "https://api.cerebras.ai/v1", "CEREBRAS_API_KEY"),
        (
            "fireworks",
            "https://api.fireworks.ai/inference/v1",
            "FIREWORKS_API_KEY",
        ),
        (
            "perplexity",
            "https://api.perplexity.ai",
            "PERPLEXITY_API_KEY",
        ),
        ("cohere", "https://api.cohere.com/v2", "COHERE_API_KEY"),
        (
            "huggingface",
            "https://api-inference.huggingface.co/v1",
            "HF_API_KEY",
        ),
        // --- China ---
        (
            "siliconflow",
            "https://api.siliconflow.cn/v1",
            "SILICONFLOW_API_KEY",
        ),
        (
            "qwen",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "DASHSCOPE_API_KEY",
        ),
        ("kimi", "https://api.moonshot.cn/v1", "MOONSHOT_API_KEY"), // Kimi = Moonshot
        ("moonshot", "https://api.moonshot.cn/v1", "MOONSHOT_API_KEY"),
        (
            "zhipu",
            "https://open.bigmodel.cn/api/paas/v4",
            "ZHIPU_API_KEY",
        ),
        (
            "baichuan",
            "https://api.baichuan-ai.com/v1",
            "BAICHUAN_API_KEY",
        ),
        ("minimax", "https://api.minimaxi.com/v1", "MINIMAX_API_KEY"),
        ("stepfun", "https://api.stepfun.com/v1", "STEPFUN_API_KEY"),
        ("lingyi", "https://api.lingyiwanwu.com/v1", "LINGYI_API_KEY"),
        (
            "baidu",
            "https://qianfan.baidubce.com/v2",
            "QIANFAN_API_KEY",
        ),
        (
            "gaterouter",
            "https://api.gaterouter.ai/openai/v1",
            "GATEROUTER_API_KEY",
        ),
        (
            "infini",
            "https://cloud.infini-ai.com/maas/v1",
            "INFINI_API_KEY",
        ),
    ];

    for (name, base_url, env_key) in compat_providers {
        if !registry.names().contains(&name)
            && let Ok(key) = std::env::var(env_key)
        {
            registry.register(
                name,
                Arc::new(OpenAiProvider::with_base_url(base_url, Some(key))),
            );
        }
    }

    // Doubao / ByteDance — uses OpenAI Responses API format.
    if !registry.names().contains(&"doubao") {
        if let Ok(key) = std::env::var("ARK_API_KEY").or_else(|_| std::env::var("DOUBAO_API_KEY")) {
            registry.register(
                "doubao",
                Arc::new(OpenAiProvider::responses(
                    "https://ark.cn-beijing.volces.com/api/v3",
                    Some(key),
                )),
            );
        }
    }
    if !registry.names().contains(&"bytedance") {
        if let Ok(key) = std::env::var("ARK_API_KEY").or_else(|_| std::env::var("DOUBAO_API_KEY")) {
            registry.register(
                "bytedance",
                Arc::new(OpenAiProvider::responses(
                    "https://ark.cn-beijing.volces.com/api/v3",
                    Some(key),
                )),
            );
        }
    }

    // Ollama (no API key needed).
    if !registry.names().contains(&"ollama") {
        registry.register(
            "ollama",
            Arc::new(OpenAiProvider::with_base_url(
                "http://localhost:11434",
                None,
            )),
        );
    }

    // rsclaw-server — internal multi-provider gateway. Speaks OpenAI Chat
    // Completions; clients send `Authorization: Bearer <client_key>` and
    // upstream routing happens server-side.
    //
    //   RSCLAW_SERVER_KEY  — required, matches a `[[client_keys]]` entry
    //                        in rsclaw-server's config.toml
    //   RSCLAW_SERVER_URL  — optional override (default: http://localhost:8090/v1)
    //
    // `nonempty_env` (not `std::env::var(...).ok()`) so a placeholder
    // `RSCLAW_SERVER_KEY=` in a dotenv template doesn't register the
    // provider with an empty bearer (silent 401s upstream), and a blank
    // `RSCLAW_SERVER_URL=` doesn't defeat the localhost default by
    // returning `Ok("")` and falling through to register an
    // unparseable empty base URL. Same rationale as the rsclaw block
    // below.
    if !registry.names().contains(&"rsclaw_server")
        && let Some(key) = nonempty_env("RSCLAW_SERVER_KEY")
    {
        let url = nonempty_env("RSCLAW_SERVER_URL")
            .unwrap_or_else(|| "http://localhost:8090/v1".to_string());
        registry.register(
            "rsclaw_server",
            Arc::new(OpenAiProvider::with_base_url(url, Some(key))),
        );
    }

    // rsclaw — kvCacheMode=2 incremental session protocol (rsclaw-protocol.md).
    // Distinct from `rsclaw_server` above: that one speaks OpenAI Chat for
    // mode 0/1 traffic; this one speaks the stateful session protocol and
    // rejects requests with kv_cache_mode != 2.
    //
    //   RSCLAW_KEY  — bearer token (optional if rsclaw-server has auth disabled)
    //   RSCLAW_URL  — full base URL including the `/v1/agent` mount
    //                 (default: http://localhost:8090/v1/agent)
    if !registry.names().contains(&"rsclaw") {
        // `std::env::var(...).ok()` returns `Some("")` when an env var
        // is *set but blank* (e.g. `RSCLAW_KEY=` in a dotenv file used
        // as a placeholder/template). `Some("")` is truthy in
        // `.or_else(...)`, so the fallback chain
        // `RSCLAW_KEY` → `RSCLAW_SERVER_KEY` short-circuits on the
        // empty earlier value and never reaches the populated later
        // one — leaving the gateway with no bearer despite the user
        // having set one. `nonempty_env` skips the empty-set case so
        // the chain composes correctly.
        let key = nonempty_env("RSCLAW_KEY").or_else(|| nonempty_env("RSCLAW_SERVER_KEY"));
        let url = nonempty_env("RSCLAW_URL")
            .unwrap_or_else(|| crate::provider::rsclaw::RSCLAW_DEFAULT_BASE.to_string());
        registry.register("rsclaw", Arc::new(RsclawProvider::new(url, key)));
    }

    // Wire up model aliases from agents.defaults.models.
    if let Some(models) = &config.agents.defaults.models {
        let mut aliases = HashMap::new();
        for (model_key, alias_def) in models {
            if let Some(ref target) = alias_def.alias {
                aliases.insert(model_key.clone(), target.clone());
            } else if let Some(ref target_model) = alias_def.model {
                // model field: "deepseek/deepseek-v3" -> provider is first segment
                if let Some((prov, _)) = target_model.split_once('/') {
                    aliases.insert(model_key.clone(), prov.to_owned());
                }
            }
        }
        if !aliases.is_empty() {
            info!("{} model alias(es) configured", aliases.len());
            registry.set_model_aliases(aliases);
        }
    }

    registry
}

/// Read an env var and treat both *unset* and *set-but-blank* as
/// absent. Mirrors `std::env::var(name).ok()` for the absent case but
/// adds whitespace-aware blank detection so callers can chain
/// fallbacks (`A → B → C`) without an empty earlier value vetoing the
/// rest. See the rsclaw block above for the concrete short-circuit
/// scenario this prevents.
fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonempty_env_skips_unset_set_blank_and_whitespace_only() {
        // Use unique names so parallel tests don't race each other.
        // The point of the test is the *behaviour shape*, not the
        // env-var lookup itself — exercise via SAFETY-ed set/remove.
        let unset = "RSCLAW_TEST_NONEMPTY_UNSET_8d2f1c";
        let blank = "RSCLAW_TEST_NONEMPTY_BLANK_8d2f1c";
        let spaces = "RSCLAW_TEST_NONEMPTY_SPACES_8d2f1c";
        let real = "RSCLAW_TEST_NONEMPTY_REAL_8d2f1c";
        // SAFETY: env mutation is process-global. Names are unique so
        // no other test in this crate observes them.
        unsafe {
            std::env::remove_var(unset);
            std::env::set_var(blank, "");
            std::env::set_var(spaces, "   \t\n");
            std::env::set_var(real, "  sk-real  ");
        }
        assert_eq!(nonempty_env(unset), None, "unset");
        assert_eq!(nonempty_env(blank), None, "set-blank");
        assert_eq!(nonempty_env(spaces), None, "whitespace-only");
        // Trimming is the *provider's* job (provider/rsclaw.rs), not
        // ours — return the raw string so the caller can decide. We
        // only filter on the trim *result* to detect blank.
        assert_eq!(nonempty_env(real).as_deref(), Some("  sk-real  "));
        unsafe {
            std::env::remove_var(blank);
            std::env::remove_var(spaces);
            std::env::remove_var(real);
        }
    }
}
