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

            let profiles = self.order.get(provider_name).cloned().unwrap_or_else(|| {
                vec!["default".to_owned()]
            });

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

                match provider.stream(req.clone()).await {
                    Ok(stream) => {
                        self.failure_counts.remove(profile_id);
                        info!(
                            provider = provider_name,
                            model = model_id,
                            profile = profile_id,
                            "LLM call succeeded"
                        );
                        return Ok(stream);
                    }
                    Err(e) if is_rate_limit(&e) || is_auth_error(&e) => {
                        let attempt = self.hit_count(profile_id);
                        let delay = backoff_delay(attempt, &self.retry)
                            .max(MIN_COOLDOWN)
                            .min(MAX_COOLDOWN);
                        warn!(
                            provider = provider_name,
                            profile = profile_id,
                            error = %e,
                            ?delay,
                            attempt,
                            "rate limit / auth error — cooling down profile"
                        );
                        self.set_cooldown(profile_id, delay);
                        // continue to next profile
                    }
                    Err(e) => {
                        // Non-retryable error — propagate immediately.
                        return Err(e);
                    }
                }
            }
        }

        Err(anyhow!("LLM service unavailable — provider rate limited or API key invalid. Please check your provider configuration or try again later."))
    }

    fn is_cooling_down(&self, profile_id: &str) -> bool {
        self.cooldowns
            .get(profile_id)
            .is_some_and(|&until| Instant::now() < until)
    }

    fn set_cooldown(&mut self, profile_id: &str, delay: Duration) {
        self.cooldowns
            .insert(profile_id.to_owned(), Instant::now() + delay);
        *self.failure_counts.entry(profile_id.to_owned()).or_insert(0) += 1;
    }

    /// Returns the current consecutive failure count for a profile (0 = no recent failures).
    fn hit_count(&self, profile_id: &str) -> u32 {
        self.failure_counts.get(profile_id).copied().unwrap_or(0)
    }
}

fn is_rate_limit(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("429") || msg.contains("rate limit") || msg.contains("too many requests")
}

fn is_auth_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("401") || msg.contains("unauthorized") || msg.contains("invalid api key")
}
