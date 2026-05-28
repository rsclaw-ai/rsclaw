//! Provider failover manager.
//!
//! Two layers of retry, composed:
//!   1. **Model chain** (this file): iterate `[req.model, ...req.fallback_models,
//!      ...self.fallbacks]` dedup'd; skip entries whose per-model health is
//!      Cooling-not-expired or Disabled (see `provider::health`).
//!   2. **Profile cooldown** (per chain entry): within a single model the
//!      manager rotates through configured profile ids (different API keys
//!      for the same provider — `auth.order[provider]`). 429/auth blips on
//!      one key flip to the next.
//!
//! On any call's outcome the manager updates **both** layers:
//!   - profile cooldown for transient 429 / auth flaps on a specific key
//!   - per-model health for chain-level decisions (Disabled on
//!     balance/key/model-missing; Cooling on transient/rate-limit)

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use tracing::{info, warn};

use super::{
    LlmRequest, LlmStream, RetryConfig, backoff_delay,
    health::{ErrorKind, ProviderHealthRegistry, classify_error},
    registry::ProviderRegistry,
};

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
    /// Global emergency fallback model list (last resort after per-call
    /// `req.fallback_models`). Kept separate so callers without a chain
    /// still benefit from the gateway-wide safety net.
    fallbacks: Vec<String>,
    /// retry / back-off configuration (agents.md §22)
    retry: RetryConfig,
    /// Shared per-model health table — same `Arc` is held by `AppState`,
    /// so `/api/v1/models/health` reads what this loop writes. Lazy init
    /// happens in `build_chain` → `model_health.ensure(...)`.
    model_health: ProviderHealthRegistry,
}

impl FailoverManager {
    pub fn new(
        order: HashMap<String, Vec<String>>,
        api_keys: HashMap<String, String>,
        fallbacks: Vec<String>,
        model_health: ProviderHealthRegistry,
    ) -> Self {
        Self {
            order,
            api_keys,
            fallbacks,
            cooldowns: HashMap::new(),
            failure_counts: HashMap::new(),
            retry: RetryConfig::default(),
            model_health,
        }
    }

    /// Build the full ordered chain from `req.model + req.fallback_models +
    /// self.fallbacks`, deduplicated. Empties dropped.
    fn build_chain(&self, req: &LlmRequest) -> Vec<String> {
        let mut out: Vec<String> =
            Vec::with_capacity(1 + req.fallback_models.len() + self.fallbacks.len());
        let mut seen = std::collections::HashSet::new();
        let mut push = |s: &str, out: &mut Vec<String>| {
            let t = s.trim();
            if !t.is_empty() && seen.insert(t.to_owned()) {
                out.push(t.to_owned());
            }
        };
        push(&req.model, &mut out);
        for m in &req.fallback_models {
            push(m, &mut out);
        }
        for m in &self.fallbacks {
            push(m, &mut out);
        }
        out
    }

    /// Execute an LLM request with model-chain + per-profile failover.
    pub async fn call(
        &mut self,
        mut req: LlmRequest,
        registry: &ProviderRegistry,
    ) -> Result<LlmStream> {
        let chain = self.build_chain(&req);
        if chain.is_empty() {
            return Err(anyhow!("LLM request has no model resolved"));
        }
        self.model_health.ensure(&chain);

        let mut last_error: Option<anyhow::Error> = None;

        for model_str in &chain {
            // ── Chain-level health gate ───────────────────────────────────
            // Skip entries the shared health table has marked Disabled or
            // whose Cooling window hasn't expired. The single-model
            // back-compat path (chain.len() == 1) always passes this when
            // health is pristine — preserving "not worse than before".
            if !self.model_health.is_callable(model_str) {
                continue;
            }

            let (provider_name, model_id) = registry.resolve_model(model_str);
            req.model = model_id.to_owned();

            let profiles = self
                .order
                .get(provider_name)
                .cloned()
                .unwrap_or_else(|| vec!["default".to_owned()]);

            let outcome = self
                .try_model_with_profiles(provider_name, model_id, &profiles, &mut req, registry)
                .await;

            match outcome {
                ChainStep::Success(stream) => {
                    self.model_health.record_success(model_str);
                    return Ok(stream);
                }
                ChainStep::PropagateError(e) => return Err(e),
                ChainStep::TryNextModel(e) => {
                    let kind = classify_error(&e);
                    if kind == ErrorKind::Unknown {
                        // Ambiguous error — we can't attribute it to the model
                        // being unhealthy (it may be a transient hiccup, a
                        // malformed/empty response, or something model-specific
                        // that the next request recovers from). Advance the
                        // chain for THIS request so the user still gets an
                        // answer, but do NOT cool the model down — otherwise a
                        // flaky-but-usable primary (e.g. agent-v1) gets parked
                        // and every later request is forced onto the fallback.
                        info!(
                            model = %model_str,
                            kind = ?kind,
                            "ambiguous error; advancing chain for this request without marking the model unavailable"
                        );
                    } else {
                        let body = format!("{e:#}");
                        let truncated = crate::util::truncate_str(&body, 200).to_owned();
                        self.model_health
                            .record_failure(model_str, kind.clone(), truncated);
                        info!(
                            model = %model_str,
                            kind = ?kind,
                            "model marked unavailable, advancing chain"
                        );
                    }
                    last_error = Some(e);
                    continue;
                }
                ChainStep::AllProfilesCooling => {
                    continue;
                }
            }
        }

        Err(match last_error {
            Some(e) => anyhow!(
                "LLM chain exhausted ({} models tried). Last error: {e:#}",
                chain.len()
            ),
            None => anyhow!(
                "LLM chain exhausted ({} models, all cooling). Wait for cooldown, reset health, or check provider status.",
                chain.len()
            ),
        })
    }

    /// Inner profile loop for a single chain entry. Returns a `ChainStep`
    /// telling the outer loop whether to propagate, advance, or stop.
    async fn try_model_with_profiles(
        &mut self,
        provider_name: &str,
        model_id: &str,
        profiles: &[String],
        req: &mut LlmRequest,
        registry: &ProviderRegistry,
    ) -> ChainStep {
        let mut any_profile_called = false;
        let mut last_err: Option<anyhow::Error> = None;

        for profile_id in profiles {
            if self.is_cooling_down(profile_id) {
                warn!(profile = profile_id, "profile is cooling down, skipping");
                continue;
            }

            let provider = match registry.get(provider_name) {
                Ok(p) => p,
                Err(e) => {
                    warn!(provider = provider_name, "provider not found: {e}");
                    return ChainStep::TryNextModel(anyhow!(
                        "provider '{provider_name}' not found: {e}"
                    ));
                }
            };
            let provider_api = provider.name();

            any_profile_called = true;
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
                        return ChainStep::Success(stream);
                    }
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
                        return ChainStep::PropagateError(anyhow!(
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
                        last_err = Some(e);
                        break; // try next profile
                    }
                    Err(e) => {
                        // Classify to decide: bubble or advance chain.
                        let kind = classify_error(&e);
                        match kind {
                            ErrorKind::Balance
                            | ErrorKind::ModelMissing
                            | ErrorKind::Auth
                            | ErrorKind::RateLimit
                            | ErrorKind::Transient
                            | ErrorKind::Unknown => {
                                return ChainStep::TryNextModel(e);
                            }
                            // BadRequest: our serialization fault, not the
                            // model's — bubble to caller.
                            // ContextExceeded: session too big for this
                            // backend; failing over to another model just
                            // masks it (a bigger-ctx fallback "works" but
                            // slowly). Bubble so the agent loop can compact /
                            // recreate the session and retry the SAME model.
                            ErrorKind::BadRequest | ErrorKind::ContextExceeded => {
                                return ChainStep::PropagateError(e);
                            }
                        }
                    }
                }
            }
        }

        if !any_profile_called {
            return ChainStep::AllProfilesCooling;
        }
        match last_err {
            Some(e) => ChainStep::TryNextModel(e),
            None => ChainStep::AllProfilesCooling,
        }
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

    fn hit_count(&self, profile_id: &str) -> u32 {
        self.failure_counts.get(profile_id).copied().unwrap_or(0)
    }
}

/// Outcome of the inner per-profile loop, telling the outer chain loop
/// what to do next.
enum ChainStep {
    Success(LlmStream),
    PropagateError(anyhow::Error),
    TryNextModel(anyhow::Error),
    AllProfilesCooling,
}

/// Detects rejection caused by the request's output-token budget exceeding the
/// model's context window or the account tier's hard ceiling.
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
