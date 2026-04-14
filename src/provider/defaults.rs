//! Provider default definitions — base URLs and auth styles.
//!
//! **Resolution order** (highest priority first):
//!   1. `defaults.toml` at `$RSCLAW_BASE_DIR/defaults.toml` (user-editable)
//!   2. `defaults.toml` embedded at compile time
//!   3. Hardcoded fallback in this file
//!
//! This is the **single source of truth** for provider URLs. All other code
//! (`gateway/startup.rs`, `server/mod.rs`) should call [`resolve_base_url`]
//! instead of hardcoding URLs.

use std::{collections::HashMap, sync::OnceLock};

// ---------------------------------------------------------------------------
// Hardcoded fallbacks (last resort when defaults.toml is missing/corrupt)
// ---------------------------------------------------------------------------

/// Returns `(base_url, auth_style)` from hardcoded defaults.
/// `auth_style`: `"bearer"`, `"x-api-key"`, or `"none"`.
fn builtin_base_url(provider: &str) -> (&'static str, &'static str) {
    match provider {
        "anthropic"          => ("https://api.anthropic.com/v1",                       "x-api-key"),
        "openai"             => ("https://api.openai.com/v1",                         "bearer"),
        "deepseek"           => ("https://api.deepseek.com/v1",                       "bearer"),
        "qwen"               => ("https://dashscope.aliyuncs.com/compatible-mode/v1",             "bearer"),
        "doubao"             => ("https://ark.cn-beijing.volces.com/api/v3",          "bearer"),
        "minimax"            => ("https://api.minimaxi.com/v1",                       "bearer"),
        "kimi" | "moonshot"  => ("https://api.moonshot.cn/v1",                        "bearer"),
        "zhipu"              => ("https://open.bigmodel.cn/api/paas/v4",              "bearer"),
        "groq"               => ("https://api.groq.com/openai/v1",                    "bearer"),
        "grok" | "xai"       => ("https://api.x.ai/v1",                              "bearer"),
        "gemini"             => ("https://generativelanguage.googleapis.com/v1beta",  "bearer"),
        "siliconflow"        => ("https://api.siliconflow.cn/v1",                     "bearer"),
        "openrouter"         => ("https://openrouter.ai/api/v1",                      "bearer"),
        "gaterouter"         => ("https://api.gaterouter.ai/openai/v1",               "bearer"),
        "stepfun"            => ("https://api.stepfun.com/v1",                        "bearer"),
        "cerebras"           => ("https://api.cerebras.ai/v1",                        "bearer"),
        "cohere"             => ("https://api.cohere.com/v2",                         "bearer"),
        "lingyi"             => ("https://api.lingyiwanwu.com/v1",                    "bearer"),
        "mistral"            => ("https://api.mistral.ai/v1",                         "bearer"),
        "ollama"             => ("http://localhost:11434",                             "none"),
        _                    => ("",                                                  "bearer"),
    }
}

// ---------------------------------------------------------------------------
// defaults.toml cache
// ---------------------------------------------------------------------------

/// Cached provider URL table from defaults.toml: name → base_url.
fn toml_providers() -> &'static HashMap<String, String> {
    static CACHE: OnceLock<HashMap<String, String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        #[derive(serde::Deserialize)]
        struct ProvEntry {
            name: String,
            #[serde(default)]
            base_url: String,
        }
        #[derive(serde::Deserialize, Default)]
        struct Defs {
            #[serde(default)]
            providers: Vec<ProvEntry>,
        }

        let raw = crate::config::loader::load_defaults_toml();
        let defs: Defs = toml::from_str(&raw).unwrap_or_default();

        defs.providers
            .into_iter()
            .filter(|p| !p.base_url.is_empty())
            .map(|p| (p.name, p.base_url))
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolve the base URL and auth style for a provider.
///
/// Priority: `defaults.toml` → hardcoded fallback.
///
/// Returns `(base_url, auth_style)` where `auth_style` is one of
/// `"bearer"`, `"x-api-key"`, or `"none"`.
pub fn resolve_base_url(provider: &str) -> (String, &'static str) {
    let (builtin_url, auth) = builtin_base_url(provider);

    // Check defaults.toml first
    if let Some(url) = toml_providers().get(provider) {
        return (url.clone(), auth);
    }
    // Also check common aliases
    if provider == "moonshot" {
        if let Some(url) = toml_providers().get("kimi") {
            return (url.clone(), auth);
        }
    }
    if provider == "xai" {
        if let Some(url) = toml_providers().get("grok") {
            return (url.clone(), auth);
        }
    }

    (builtin_url.to_owned(), auth)
}

/// Check if a base_url already ends with a version path segment
/// (e.g. `/v1`, `/v4`, `/v1beta`).
pub fn has_version_suffix(url: &str) -> bool {
    let trimmed = url.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .is_some_and(|seg| seg.starts_with('v') && seg.chars().nth(1).is_some_and(|c| c.is_ascii_digit()))
}

/// Build the models-list URL for a provider.
///
/// - Ollama: `{base}/api/tags`
/// - Others: `{base}/models` (base_url must already include version path)
pub fn models_url(provider: &str, base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if provider == "ollama" {
        format!("{trimmed}/api/tags")
    } else {
        format!("{trimmed}/models")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_known_providers() {
        let (url, auth) = builtin_base_url("anthropic");
        assert_eq!(url, "https://api.anthropic.com/v1");
        assert_eq!(auth, "x-api-key");

        let (url, _) = builtin_base_url("openai");
        assert_eq!(url, "https://api.openai.com/v1");

        let (url, _) = builtin_base_url("zhipu");
        assert_eq!(url, "https://open.bigmodel.cn/api/paas/v4");

        let (url, auth) = builtin_base_url("ollama");
        assert_eq!(url, "http://localhost:11434");
        assert_eq!(auth, "none");
    }

    #[test]
    fn builtin_unknown_returns_empty() {
        let (url, auth) = builtin_base_url("nonexistent");
        assert_eq!(url, "");
        assert_eq!(auth, "bearer");
    }

    #[test]
    fn has_version_suffix_checks() {
        assert!(has_version_suffix("https://api.openai.com/v1"));
        assert!(has_version_suffix("https://open.bigmodel.cn/api/paas/v4"));
        assert!(has_version_suffix("https://generativelanguage.googleapis.com/v1beta"));
        assert!(has_version_suffix("https://api.cohere.com/v2/"));
        assert!(!has_version_suffix("https://api.anthropic.com"));
        assert!(!has_version_suffix("http://localhost:11434"));
        assert!(!has_version_suffix("https://api.example.com/api"));
    }

    #[test]
    fn models_url_appends_directly() {
        assert_eq!(
            models_url("openai", "https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_url("zhipu", "https://open.bigmodel.cn/api/paas/v4"),
            "https://open.bigmodel.cn/api/paas/v4/models"
        );
        assert_eq!(
            models_url("doubao", "https://ark.cn-beijing.volces.com/api/v3"),
            "https://ark.cn-beijing.volces.com/api/v3/models"
        );
        assert_eq!(
            models_url("custom", "http://macstudio.local/v1"),
            "http://macstudio.local/v1/models"
        );
        assert_eq!(
            models_url("custom", "http://macstudio.local"),
            "http://macstudio.local/models"
        );
        assert_eq!(
            models_url("ollama", "http://localhost:11434"),
            "http://localhost:11434/api/tags"
        );
    }

    #[test]
    fn resolve_falls_back_to_builtin() {
        // toml_providers() may or may not have entries, but builtin should always work
        let (url, auth) = resolve_base_url("openai");
        assert!(!url.is_empty());
        assert_eq!(auth, "bearer");
    }
}
