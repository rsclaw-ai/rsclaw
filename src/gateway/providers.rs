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
