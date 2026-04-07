//! Built-in model defaults for Chinese LLM providers.
//!
//! These defaults ensure that models have reasonable max_tokens values
//! even when users don't configure them explicitly.
//!
//! Priority:
//! 1. Explicitly configured max_tokens (user config)
//! 2. Built-in model defaults (this file)
//! 3. Global default (8192)

/// Default max_tokens for models that don't specify one.
/// Matches openclaw's DEFAULT_MODEL_MAX_TOKENS = 8192.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Model defaults including max_tokens and context_window.
#[derive(Debug, Clone, Copy)]
pub struct ModelDefaults {
    /// Maximum output tokens.
    pub max_tokens: u32,
    /// Context window size (input + output).
    pub context_window: u32,
}

/// Get built-in model defaults by provider and model ID.
///
/// Returns None if the model is not in the built-in catalog,
/// in which case the caller should use DEFAULT_MAX_TOKENS.
pub fn get_model_defaults(provider: &str, model_id: &str) -> Option<ModelDefaults> {
    // Normalize provider name
    let provider = provider.to_lowercase();
    let model_lower = model_id.to_lowercase();

    match provider.as_str() {
        // DeepSeek
        "deepseek" => match model_lower.as_str() {
            "deepseek-chat" => Some(ModelDefaults {
                max_tokens: 8192,
                context_window: 64_000,
            }),
            // Reasoning models need much larger output budget
            "deepseek-reasoner" | "deepseek-r1" => Some(ModelDefaults {
                max_tokens: 65536,
                context_window: 131_072,
            }),
            "deepseek-coder" => Some(ModelDefaults {
                max_tokens: 4096,
                context_window: 16_384,
            }),
            _ => Some(ModelDefaults {
                max_tokens: 8192,
                context_window: 64_000,
            }),
        },

        // 阿里云通义千问
        "qwen" | "dashscope" | "aliyun" => {
            // Reasoning models (QwQ) need larger output
            let max_tokens = if model_lower.contains("qwq") || model_lower.contains("reasoning") {
                32768
            } else {
                8192
            };
            let context_window = if model_lower.contains("long") {
                1_048_576
            } else if model_lower.contains("32k") {
                32_768
            } else if model_lower.contains("128k") {
                131_072
            } else {
                131_072
            };
            Some(ModelDefaults {
                max_tokens,
                context_window,
            })
        }

        // 月之暗面 Kimi
        "kimi" | "moonshot" => {
            let context_window = if model_lower.contains("128k") {
                131_072
            } else if model_lower.contains("32k") {
                32_768
            } else {
                8192
            };
            Some(ModelDefaults {
                max_tokens: 4096,
                context_window,
            })
        }

        // 智谱 GLM
        "zhipu" | "bigmodel" => Some(ModelDefaults {
            max_tokens: 4096,
            context_window: 131_072,
        }),

        // 零一万物
        "yi" | "lingyiwanwu" => Some(ModelDefaults {
            max_tokens: 4096,
            context_window: 16_384,
        }),

        // MiniMax
        "minimax" => Some(ModelDefaults {
            max_tokens: 4096,
            context_window: 245_000,
        }),

        // SiliconFlow / 硅基流动
        "siliconflow" | "silicon" => {
            // SiliconFlow hosts many models
            if model_lower.contains("deepseek-r1") || model_lower.contains("deepseek-reasoner") {
                Some(ModelDefaults {
                    max_tokens: 65536,
                    context_window: 131_072,
                })
            } else if model_lower.contains("deepseek") {
                Some(ModelDefaults {
                    max_tokens: 8192,
                    context_window: 64_000,
                })
            } else if model_lower.contains("qwq") || model_lower.contains("qwen") && model_lower.contains("reasoning") {
                Some(ModelDefaults {
                    max_tokens: 32768,
                    context_window: 131_072,
                })
            } else if model_lower.contains("qwen") {
                Some(ModelDefaults {
                    max_tokens: 8192,
                    context_window: 131_072,
                })
            } else {
                Some(ModelDefaults {
                    max_tokens: 8192,
                    context_window: 32_768,
                })
            }
        }

        // 阶跃星辰
        "stepfun" | "step" => Some(ModelDefaults {
            max_tokens: 4096,
            context_window: 8192,
        }),

        // 百川
        "baichuan" => Some(ModelDefaults {
            max_tokens: 4096,
            context_window: 32_768,
        }),

        // 讯飞星火
        "spark" | "xfyun" | "xinghuo" => Some(ModelDefaults {
            max_tokens: 4096,
            context_window: 8192,
        }),

        // 字节豆包
        "doubao" | "bytedance" => Some(ModelDefaults {
            max_tokens: 4096,
            context_window: 32_768,
        }),

        // 百度文心
        "wenxin" | "ernie" | "baidu" => Some(ModelDefaults {
            max_tokens: 2048,
            context_window: 8192,
        }),

        // OpenAI - known models
        "openai" => match model_lower.as_str() {
            "gpt-4o" | "gpt-4o-mini" => Some(ModelDefaults {
                max_tokens: 4096,
                context_window: 128_000,
            }),
            "gpt-4-turbo" | "gpt-4-0125-preview" | "gpt-4-1106-preview" => Some(ModelDefaults {
                max_tokens: 4096,
                context_window: 128_000,
            }),
            "gpt-4" => Some(ModelDefaults {
                max_tokens: 8192,
                context_window: 8192,
            }),
            "gpt-3.5-turbo" => Some(ModelDefaults {
                max_tokens: 4096,
                context_window: 16_385,
            }),
            "o1-preview" | "o1-mini" => Some(ModelDefaults {
                max_tokens: 32_768,
                context_window: 128_000,
            }),
            _ => Some(ModelDefaults {
                max_tokens: 4096,
                context_window: 8192,
            }),
        },

        // Anthropic
        "anthropic" => match model_lower.as_str() {
            m if m.starts_with("claude-opus") => Some(ModelDefaults {
                max_tokens: 4096,
                context_window: 200_000,
            }),
            m if m.starts_with("claude-sonnet") => Some(ModelDefaults {
                max_tokens: 8192,
                context_window: 200_000,
            }),
            m if m.starts_with("claude-haiku") => Some(ModelDefaults {
                max_tokens: 8192,
                context_window: 200_000,
            }),
            _ => Some(ModelDefaults {
                max_tokens: 4096,
                context_window: 200_000,
            }),
        },

        // Google Gemini
        "gemini" | "google" => Some(ModelDefaults {
            max_tokens: 8192,
            context_window: 1_048_576,
        }),

        // Groq
        "groq" => Some(ModelDefaults {
            max_tokens: 4096,
            context_window: 8192,
        }),

        // xAI Grok
        "xai" | "grok" => Some(ModelDefaults {
            max_tokens: 4096,
            context_window: 131_072,
        }),

        // Ollama - no specific defaults, use conservative values
        "ollama" => Some(ModelDefaults {
            max_tokens: 4096,
            context_window: 4096,
        }),

        // Unknown provider - no built-in defaults
        _ => None,
    }
}

/// Get max_tokens for a model, with fallback to default.
///
/// Priority:
/// 1. Explicitly configured max_tokens
/// 2. Built-in model defaults
/// 3. Global default (4096)
pub fn resolve_max_tokens(
    provider: &str,
    model_id: &str,
    configured_max_tokens: Option<u32>,
) -> u32 {
    if let Some(max_tokens) = configured_max_tokens {
        return max_tokens;
    }

    get_model_defaults(provider, model_id)
        .map(|d| d.max_tokens)
        .unwrap_or(DEFAULT_MAX_TOKENS)
}

/// Get context_window for a model, with fallback to default.
pub fn resolve_context_window(
    provider: &str,
    model_id: &str,
    configured_context_window: Option<u32>,
) -> u32 {
    if let Some(context_window) = configured_context_window {
        return context_window;
    }

    get_model_defaults(provider, model_id)
        .map(|d| d.context_window)
        .unwrap_or(8192)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deepseek_chat_defaults() {
        let defaults = get_model_defaults("deepseek", "deepseek-chat").unwrap();
        assert_eq!(defaults.max_tokens, 8192);
        assert_eq!(defaults.context_window, 64_000);
    }

    #[test]
    fn test_deepseek_reasoner_defaults() {
        // Reasoning models need larger output budget
        let defaults = get_model_defaults("deepseek", "deepseek-reasoner").unwrap();
        assert_eq!(defaults.max_tokens, 65536);
        assert_eq!(defaults.context_window, 131_072);

        let defaults_r1 = get_model_defaults("deepseek", "deepseek-r1").unwrap();
        assert_eq!(defaults_r1.max_tokens, 65536);
    }

    #[test]
    fn test_qwen_defaults() {
        let defaults = get_model_defaults("qwen", "qwen-turbo").unwrap();
        assert_eq!(defaults.max_tokens, 8192);

        let defaults_long = get_model_defaults("qwen", "qwen-long").unwrap();
        assert_eq!(defaults_long.context_window, 1_048_576);
    }

    #[test]
    fn test_qwen_reasoning_defaults() {
        // QwQ reasoning models need larger output
        let defaults = get_model_defaults("qwen", "qwq-32b").unwrap();
        assert_eq!(defaults.max_tokens, 32768);
    }

    #[test]
    fn test_kimi_defaults() {
        let defaults = get_model_defaults("kimi", "moonshot-v1-8k").unwrap();
        assert_eq!(defaults.max_tokens, 4096);
        assert_eq!(defaults.context_window, 8192);

        let defaults_128k = get_model_defaults("kimi", "moonshot-v1-128k").unwrap();
        assert_eq!(defaults_128k.context_window, 131_072);
    }

    #[test]
    fn test_unknown_provider() {
        let defaults = get_model_defaults("unknown", "some-model");
        assert!(defaults.is_none());
    }

    #[test]
    fn test_resolve_max_tokens_configured() {
        let result = resolve_max_tokens("deepseek", "deepseek-chat", Some(1000));
        assert_eq!(result, 1000);
    }

    #[test]
    fn test_resolve_max_tokens_builtin() {
        let result = resolve_max_tokens("deepseek", "deepseek-chat", None);
        assert_eq!(result, 8192);
    }

    #[test]
    fn test_resolve_max_tokens_default() {
        let result = resolve_max_tokens("unknown", "some-model", None);
        assert_eq!(result, 8192);
    }

    #[test]
    fn test_siliconflow_deepseek_r1() {
        let defaults = get_model_defaults("siliconflow", "deepseek-ai/DeepSeek-R1").unwrap();
        assert_eq!(defaults.max_tokens, 65536);
    }
}