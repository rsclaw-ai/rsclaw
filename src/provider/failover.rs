//! Provider failover manager.
//!
//! Implements the full retry/failover flow documented in AGENTS.md §12:
//!   auth.order[provider] → profile cooldown → cross-provider fallback

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use tracing::{info, warn};

use super::{LlmRequest, LlmStream, RetryConfig, backoff_delay, registry::ProviderRegistry};

/// Minimum back-off for a rate-limited profile.
const MIN_COOLDOWN: Duration = Duration::from_secs(5);
/// Maximum back-off cap.
const MAX_COOLDOWN: Duration = Duration::from_secs(300);

pub struct FailoverManager {
    /// provider_name → [profile_id, ...]  (resolution order)
    order: HashMap<String, Vec<String>>,
    /// profile_id → cooldown_until
    cooldowns: HashMap<String, Instant>,
    /// profile_id → consecutive failure count
    failure_counts: HashMap<String, u32>,
    /// profile_id → api_key
    #[allow(dead_code)]
    api_keys: HashMap<String, String>,
    /// fallback model list (provider/model strings)
    fallbacks: Vec<String>,
    /// retry / back-off configuration (agents.md §22)
    retry: RetryConfig,
}

impl FailoverManager {
    pub fn new(
        order: HashMap<String, Vec<String>>,
        api_keys: HashMap<String, String>,
        fallbacks: Vec<String>,
    ) -> Self {
        Self {
            order,
            api_keys,
            fallbacks,
            cooldowns: HashMap::new(),
            failure_counts: HashMap::new(),
            retry: RetryConfig::default(),
        }
    }

    /// Execute an LLM request with full provider/profile failover.
    pub async fn call(
        &mut self,
        mut req: LlmRequest,
        registry: &ProviderRegistry,
    ) -> Result<LlmStream> {
        let primary = req.model.clone();
        let models: Vec<String> = std::iter::once(primary)
            .chain(self.fallbacks.clone())
            .collect();

        for model_str in &models {
            let (provider_name, model_id) = registry.resolve_model(model_str);
            req.model = model_id.to_owned();

            let profiles = self
                .order
                .get(provider_name)
                .cloned()
                .unwrap_or_else(|| vec!["default".to_owned()]);

            for profile_id in &profiles {
                if self.is_cooling_down(profile_id) {
                    warn!(profile = profile_id, "profile is cooling down, skipping");
                    continue;
                }

                let provider = match registry.get(provider_name) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(provider = provider_name, "provider not found: {e}");
                        break;
                    }
                };
                // Snapshot the api protocol the resolved provider speaks
                // (`openai` / `openai-responses` / `anthropic` / `gemini` /
                // `rsclaw` / `ollama`) so every log line below can surface
                // it. Without this, `provider="doubao"` alone hides whether
                // the actual call went via Responses or Anthropic, which is
                // the exact ambiguity that masked the recent ARK 404.
                let provider_api = provider.name();

                // One profile attempt, with a single in-place retry if the
                // request is rejected for exceeding the model/tier output-token
                // ceiling. We drop `max_tokens` and let the server fall back to
                // its own model maximum — this is provider-agnostic (no need to
                // parse each backend's ceiling wording) and resolves in one shot
                // (vs. halving, which may take several rounds to get under the
                // limit).
                let mut dropped_max_tokens = false;
                loop {
                    match provider.stream(req.clone()).await {
                        Ok(stream) => {
                            self.failure_counts.remove(profile_id);
                            info!(
                                provider = provider_name,
                                api = provider_api,
                                model = model_id,
                                profile = profile_id,
                                "LLM call succeeded"
                            );
                            return Ok(stream);
                        }
                        // Output-token-limit rejection: not a rate limit and not
                        // an auth problem — cooling down or failing over won't
                        // help because every backend will reject the same
                        // oversized `max_tokens`. Try once more without it.
                        Err(e) if is_max_tokens_error(&e) => {
                            if req.max_tokens.is_some() && !dropped_max_tokens {
                                warn!(
                                    provider = provider_name,
                                    api = provider_api,
                                    profile = profile_id,
                                    error = %e,
                                    "max_tokens exceeds model/tier ceiling — dropping max_tokens and retrying once"
                                );
                                req.max_tokens = None;
                                dropped_max_tokens = true;
                                continue;
                            }
                            // Already retried without max_tokens (or there was
                            // none to drop) and it still failed — surface a
                            // clear, actionable error instead of a misleading
                            // "rate limited" message. Do NOT cool down.
                            return Err(anyhow!(
                                "LLM request rejected: output token limit exceeded. \
                                 The configured max_tokens is above this model/tier's ceiling \
                                 and retrying without it still failed. Lower max_tokens in your \
                                 config (model.max_tokens / agents.defaults). Underlying error: {e}"
                            ));
                        }
                        Err(e) if is_rate_limit(&e) || is_auth_error(&e) => {
                            let attempt = self.hit_count(profile_id);
                            let delay = backoff_delay(attempt, &self.retry)
                                .max(MIN_COOLDOWN)
                                .min(MAX_COOLDOWN);
                            warn!(
                                provider = provider_name,
                                api = provider_api,
                                profile = profile_id,
                                error = %e,
                                ?delay,
                                attempt,
                                "rate limit / auth error — cooling down profile"
                            );
                            self.set_cooldown(profile_id, delay);
                            break; // continue to next profile
                        }
                        Err(e) => {
                            // Non-retryable error — propagate immediately.
                            return Err(e);
                        }
                    }
                }
            }
        }

        Err(anyhow!(
            "LLM service unavailable — provider rate limited or API key invalid. Please check your provider configuration or try again later."
        ))
    }

    fn is_cooling_down(&self, profile_id: &str) -> bool {
        self.cooldowns
            .get(profile_id)
            .is_some_and(|&until| Instant::now() < until)
    }

    fn set_cooldown(&mut self, profile_id: &str, delay: Duration) {
        self.cooldowns
            .insert(profile_id.to_owned(), Instant::now() + delay);
        *self
            .failure_counts
            .entry(profile_id.to_owned())
            .or_insert(0) += 1;
    }

    /// Returns the current consecutive failure count for a profile (0 = no
    /// recent failures).
    fn hit_count(&self, profile_id: &str) -> u32 {
        self.failure_counts.get(profile_id).copied().unwrap_or(0)
    }
}

/// Detects rejection caused by the request's output-token budget exceeding the
/// model's context window or the account tier's hard ceiling — distinct from a
/// transient rate limit. Matches the common wording across backends:
///   - rsclaw:    `max_tokens=N exceeds tier "..." ceiling (M)`
///   - OpenAI:    `maximum context length is N tokens` /
///     `context_length_exceeded`
///   - Anthropic: `max_tokens: N > M, which is the maximum ...`
///
/// The remedy is to drop `max_tokens` and retry; cooling down or failing over
/// does not help because every backend rejects the same oversized value.
fn is_max_tokens_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("max_tokens")
        || msg.contains("context_length_exceeded")
        || msg.contains("maximum context length")
        || msg.contains("context length exceeded")
}

fn is_rate_limit(e: &anyhow::Error) -> bool {
    // A 429 carrying a max_tokens/context-ceiling body is a configuration
    // error, not a transient rate limit — classify it as the former so we
    // don't cool the profile down and retry pointlessly.
    if is_max_tokens_error(e) {
        return false;
    }
    let msg = e.to_string().to_lowercase();
    msg.contains("429") || msg.contains("rate limit") || msg.contains("too many requests")
}

fn is_auth_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("401") || msg.contains("unauthorized") || msg.contains("invalid api key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rsclaw_tier_ceiling_is_config_error_not_rate_limit() {
        let e = anyhow!(
            "rsclaw turn failed 429 Too Many Requests: {{\"error\":{{\"type\":\
             \"quota_exceeded\",\"message\":\"max_tokens=30000 exceeds tier \
             \\\"vip-3\\\" ceiling (16384). Lower max_tokens or upgrade tier.\"}}}}"
        );
        assert!(is_max_tokens_error(&e));
        assert!(
            !is_rate_limit(&e),
            "a max_tokens-ceiling 429 must not be treated as a transient rate limit"
        );
    }

    #[test]
    fn openai_context_length_is_max_tokens_error() {
        let e = anyhow!(
            "This model's maximum context length is 16385 tokens, however you requested 30000"
        );
        assert!(is_max_tokens_error(&e));
        assert!(!is_rate_limit(&e));
    }

    #[test]
    fn openai_context_length_exceeded_code() {
        let e = anyhow!("error code: context_length_exceeded");
        assert!(is_max_tokens_error(&e));
    }

    #[test]
    fn anthropic_max_tokens_too_large() {
        let e = anyhow!("max_tokens: 30000 > 8192, which is the maximum allowed for this model");
        assert!(is_max_tokens_error(&e));
        assert!(!is_rate_limit(&e));
    }

    #[test]
    fn genuine_rate_limit_still_cools_down() {
        let e = anyhow!("429 Too Many Requests: rate limit exceeded, please retry after 1s");
        assert!(is_rate_limit(&e));
        assert!(!is_max_tokens_error(&e));
    }

    #[test]
    fn auth_error_is_not_max_tokens() {
        let e = anyhow!("401 Unauthorized: invalid api key");
        assert!(is_auth_error(&e));
        assert!(!is_max_tokens_error(&e));
    }
}
