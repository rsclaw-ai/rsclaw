//! Per-model health tracking for the model-array failover system.
//!
//! Each `ModelConfig` field that supports a chain (primary / flash / vision
//! / image / video) is paired with a `ChainHealth` at runtime. The state
//! machine routes each call through the chain in order, demoting failing
//! models to `Cooling` (auto-recovers after backoff) or `Disabled`
//! (permanent — needs operator intervention).
//!
//! Design choices, locked in with the user:
//! - Status is **runtime-only**; never persisted to rsclaw.json5. The config
//!   stays declarative (`primary: "doubao/x"` or `primary: [a, b]`) and the
//!   user sees what they wrote.
//! - **Disabled never self-heals** automatically. Auto-probing a model that
//!   said "insufficient_quota" would just burn another tiny API charge per
//!   probe — `Disabled` is the system's way of saying "stop calling this
//!   thing until a human checks the balance / key / model id".
//! - Restart resets all state (no on-disk persistence). Simple and avoids
//!   the redb dance for what's essentially short-term volatile data.

use std::time::{Duration, Instant};

/// Classification of an LLM call failure — drives the state transition.
/// `Transient` keeps the model in the rotation (cooldown then retry);
/// `Fatal` takes it out until the operator resets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// 429 rate limit — exponential cooldown, recovers automatically.
    RateLimit,
    /// 5xx / network timeout / connect failure — short cooldown.
    Transient,
    /// Persistent 401/403 — wrong/revoked key. Disabled until reset.
    Auth,
    /// 402 / "insufficient_quota" / "余额不足" / "balance" body match.
    /// Disabled, no auto-retry (every probe would burn another charge).
    Balance,
    /// 404 model not found — id is wrong or model deprecated. Disabled.
    ModelMissing,
    /// 400/422 with a request-shape problem unrelated to the model
    /// (max_tokens overage etc.). NOT a model fault — caller handles.
    BadRequest,
    /// Default bucket for unrecognised errors. Treated as Transient so the
    /// chain still tries the next model, but flagged in logs so we can
    /// extend `classify_error` later.
    Unknown,
}

impl ErrorKind {
    /// Should this error take the model out of rotation permanently?
    pub fn is_disabling(&self) -> bool {
        matches!(self, Self::Auth | Self::Balance | Self::ModelMissing)
    }
}

/// State machine for a single model in a chain. Mutated in place by the
/// FailoverManager on each call result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    /// Default — eligible for the next call.
    Healthy,
    /// Temporarily skipped until `until` passes. After expiry the next
    /// call retries it; success → Healthy + consecutive_failures cleared.
    Cooling { until: Instant },
    /// Permanently skipped. Only an explicit reset (CLI / API / config
    /// reload that drops this id) clears it.
    Disabled { reason: String },
}

/// Health record for one model id within a chain.
#[derive(Debug, Clone)]
pub struct ModelHealth {
    pub model: String,
    pub status: ModelStatus,
    /// Last observed error body / message. Used by `/models/health` and
    /// surfaced in CLI listings.
    pub last_error: Option<String>,
    /// Counts consecutive transient/auth failures. Auth flips to
    /// Disabled after `AUTH_DISABLE_AFTER` strikes — gives the user one
    /// expired-key-cache-miss leeway before locking the model out.
    pub consecutive_failures: u32,
}

impl ModelHealth {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            status: ModelStatus::Healthy,
            last_error: None,
            consecutive_failures: 0,
        }
    }

    /// True if a caller may attempt this model right now.
    pub fn is_callable(&self) -> bool {
        match &self.status {
            ModelStatus::Healthy => true,
            ModelStatus::Cooling { until } => Instant::now() >= *until,
            ModelStatus::Disabled { .. } => false,
        }
    }

    /// Apply a successful call result — reset to Healthy.
    pub fn record_success(&mut self) {
        self.status = ModelStatus::Healthy;
        self.last_error = None;
        self.consecutive_failures = 0;
    }

    /// Apply a failure: classify + transition status + bump counters.
    /// `now` is injected so tests can pin time.
    pub fn record_failure(&mut self, kind: ErrorKind, body_snippet: String, now: Instant) {
        self.last_error = Some(body_snippet);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);

        if kind.is_disabling() {
            // Auth gets a short grace window (rotated keys etc.); Balance
            // and ModelMissing flip immediately because a single hit
            // strongly implies user-side config drift.
            let lock_now = match kind {
                ErrorKind::Auth => self.consecutive_failures >= AUTH_DISABLE_AFTER,
                _ => true,
            };
            if lock_now {
                self.status = ModelStatus::Disabled {
                    reason: format!("{kind:?}"),
                };
                return;
            }
        }

        self.status = ModelStatus::Cooling {
            until: now + cooling_backoff(self.consecutive_failures, kind),
        };
    }

    /// Manual recovery — flips Disabled back to Healthy, clears counters.
    /// Called from the `/api/v1/models/health/reset` endpoint and the
    /// `rsclaw models health reset <model>` CLI.
    pub fn reset(&mut self) {
        self.status = ModelStatus::Healthy;
        self.consecutive_failures = 0;
        self.last_error = None;
    }
}

/// Health state for an entire chain. Iterated by `FailoverManager` to pick
/// the next callable entry; mutated on each call result.
#[derive(Debug, Clone, Default)]
pub struct ChainHealth {
    pub entries: Vec<ModelHealth>,
}

impl ChainHealth {
    /// Build from a slice of model ids — fresh chain, every entry Healthy.
    pub fn from_chain<I, S>(models: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            entries: models.into_iter().map(ModelHealth::new).collect(),
        }
    }

    /// First model that's currently callable, or `None` if the chain is
    /// fully exhausted (every entry Disabled or Cooling not yet expired).
    pub fn next_callable(&self) -> Option<&ModelHealth> {
        self.entries.iter().find(|e| e.is_callable())
    }

    /// Mutable lookup by model id — used by the manager to update health
    /// after a call. Returns None for unknown ids (shouldn't happen in
    /// practice; manager constructs the chain itself).
    pub fn get_mut(&mut self, model: &str) -> Option<&mut ModelHealth> {
        self.entries.iter_mut().find(|e| e.model == model)
    }

    /// Snapshot for telemetry / endpoint serialization.
    pub fn snapshot(&self) -> Vec<(String, ModelStatus, Option<String>, u32)> {
        self.entries
            .iter()
            .map(|e| {
                (
                    e.model.clone(),
                    e.status.clone(),
                    e.last_error.clone(),
                    e.consecutive_failures,
                )
            })
            .collect()
    }

    /// True if every entry is non-callable — caller can decide whether to
    /// fall through to a separate emergency chain (primary's legacy
    /// `fallbacks` list) or bail.
    pub fn all_unavailable(&self) -> bool {
        !self.entries.is_empty() && self.entries.iter().all(|e| !e.is_callable())
    }
}

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Auth failures absorbed before flipping to Disabled. 3 = one cached-key
/// race + one operator rotation slip + one real failure. Beyond that, the
/// signal is strong enough to lock.
pub const AUTH_DISABLE_AFTER: u32 = 3;

/// Cap on cooldown duration so a long-running gateway doesn't end up with
/// a 6-hour Cooling window after a bad weekend.
pub const MAX_COOLDOWN: Duration = Duration::from_secs(3600);

/// Map (failure count, error kind) → cooldown duration. RateLimit starts
/// at 30s and doubles to MAX. Transient starts at 10s (5xx flaps shouldn't
/// dominate a chain). Unknown follows Transient's curve.
pub fn cooling_backoff(consecutive: u32, kind: ErrorKind) -> Duration {
    let base = match kind {
        ErrorKind::RateLimit => 30u64,
        ErrorKind::Transient | ErrorKind::Unknown => 10u64,
        ErrorKind::BadRequest => 5u64,
        // Disabling kinds never get here (early-returned in record_failure).
        ErrorKind::Auth | ErrorKind::Balance | ErrorKind::ModelMissing => 60u64,
    };
    let exponent = consecutive.saturating_sub(1).min(6); // cap doubling at 64×
    let secs = base.saturating_mul(1u64 << exponent);
    Duration::from_secs(secs).min(MAX_COOLDOWN)
}

// ---------------------------------------------------------------------------
// Error classifier
// ---------------------------------------------------------------------------

/// Categorise an anyhow error coming back from a provider's `stream()`
/// into an `ErrorKind`. Pattern-matches against the message body that
/// `openai.rs`, `anthropic.rs`, etc. produce when bubbling up upstream
/// failures (each one does `anyhow::bail!("... error {status}: {body}")`).
///
/// Tested against real fixtures: see `tests` module at the bottom of this
/// file.
pub fn classify_error(err: &anyhow::Error) -> ErrorKind {
    let s = format!("{err:#}");
    classify_str(&s)
}

/// Same as `classify_error` but operates on the message string directly —
/// keeps the classification logic testable without manufacturing
/// anyhow::Error values.
pub fn classify_str(s: &str) -> ErrorKind {
    let lower = s.to_lowercase();

    // -------- Balance / quota — strongest signal, check first --------
    // Volcengine Ark: "AccountOverdueError" body, sometimes status 402.
    // OpenAI: "insufficient_quota" / "billing".
    // Anthropic: "credit_balance_too_low".
    // Chinese error messages from doubao / qwen: "余额不足".
    if lower.contains("insufficient_quota")
        || lower.contains("insufficient quota")
        || lower.contains("credit_balance_too_low")
        || lower.contains("accountoverdue")
        || lower.contains("balance_not_enough")
        || lower.contains("balance not enough")
        || s.contains("余额不足")
        || s.contains("额度不足")
        || s.contains("欠费")
        || (lower.contains("402") && (lower.contains("payment") || lower.contains("balance")))
    {
        return ErrorKind::Balance;
    }

    // -------- Model not found / deprecated --------
    // OpenAI: "model_not_found" / "does not exist".
    // Anthropic: "not_found_error" with model in body.
    // Volcengine: "ModelNotOpen" / "EndpointIsNotEnabled".
    if lower.contains("model_not_found")
        || lower.contains("model not found")
        || lower.contains("does not exist or you do not have access")
        || lower.contains("modelnotopen")
        || lower.contains("endpointisnotenabled")
        || lower.contains("invalid model")
        || (lower.contains("not_found_error") && lower.contains("model"))
    {
        return ErrorKind::ModelMissing;
    }

    // -------- Auth — invalid key / forbidden --------
    if lower.contains("401")
        || lower.contains("403")
        || lower.contains("invalid_api_key")
        || lower.contains("invalid api key")
        || lower.contains("authentication_error")
        || lower.contains("unauthorized")
        || lower.contains("permission_denied")
        || lower.contains("authentication fails")
    {
        return ErrorKind::Auth;
    }

    // -------- Rate limit --------
    if lower.contains("429")
        || lower.contains("rate_limit")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("ratelimit")
    {
        return ErrorKind::RateLimit;
    }

    // -------- Bad request that's NOT a model fault --------
    // max_tokens overage already handled separately upstream — but if it
    // slips through, mark it BadRequest so the failover doesn't penalise
    // the model for our serialization mistake.
    if lower.contains("max_tokens") && (lower.contains("400") || lower.contains("exceed")) {
        return ErrorKind::BadRequest;
    }

    // -------- 5xx / transient --------
    if lower.contains("500")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("504")
        || lower.contains("overloaded")
        || lower.contains("server_error")
        || lower.contains("internal server error")
        || lower.contains("gateway timeout")
        || lower.contains("connection failed")
        || lower.contains("connect error")
        || lower.contains("timeout")
        || lower.contains("timed out")
    {
        return ErrorKind::Transient;
    }

    ErrorKind::Unknown
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_balance_doubao() {
        // Volcengine Ark 余额不足 — real body shape (Chinese message).
        let body = r#"OpenAI API error 402 Payment Required: {"error":{"code":"AccountOverdueError","message":"账户欠费,请充值后重试"}}"#;
        assert_eq!(classify_str(body), ErrorKind::Balance);
    }

    #[test]
    fn classify_balance_openai() {
        let body = r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota"}}"#;
        assert_eq!(classify_str(body), ErrorKind::Balance);
    }

    #[test]
    fn classify_balance_anthropic() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Claude API"}}"#;
        assert_eq!(classify_str(body), ErrorKind::Balance);
    }

    #[test]
    fn classify_balance_zh() {
        let body = r#"call failed: 余额不足，请充值"#;
        assert_eq!(classify_str(body), ErrorKind::Balance);
    }

    #[test]
    fn classify_auth_401() {
        let body = r#"OpenAI API error 401 Unauthorized: {"error":{"message":"Incorrect API key"}}"#;
        assert_eq!(classify_str(body), ErrorKind::Auth);
    }

    #[test]
    fn classify_auth_invalid_key() {
        let body = r#"{"error":{"type":"invalid_api_key","message":"..."}}"#;
        assert_eq!(classify_str(body), ErrorKind::Auth);
    }

    #[test]
    fn classify_rate_limit_429() {
        let body = r#"OpenAI API error 429 Too Many Requests"#;
        assert_eq!(classify_str(body), ErrorKind::RateLimit);
    }

    #[test]
    fn classify_model_missing() {
        let body = r#"{"error":{"code":"model_not_found","message":"The model gpt-5 does not exist"}}"#;
        assert_eq!(classify_str(body), ErrorKind::ModelMissing);
    }

    #[test]
    fn classify_model_missing_volcengine() {
        let body = r#"{"error":{"code":"EndpointIsNotEnabled","message":"endpoint is not enabled"}}"#;
        assert_eq!(classify_str(body), ErrorKind::ModelMissing);
    }

    #[test]
    fn classify_transient_503() {
        let body = r#"upstream 503 Service Unavailable: overloaded"#;
        assert_eq!(classify_str(body), ErrorKind::Transient);
    }

    #[test]
    fn classify_transient_timeout() {
        assert_eq!(classify_str("connection failed: timed out"), ErrorKind::Transient);
    }

    #[test]
    fn classify_bad_request_max_tokens() {
        let body = r#"400 Bad Request: max_tokens exceeds model ceiling"#;
        assert_eq!(classify_str(body), ErrorKind::BadRequest);
    }

    #[test]
    fn classify_unknown_falls_through() {
        let body = r#"unrecognised gibberish from upstream"#;
        assert_eq!(classify_str(body), ErrorKind::Unknown);
    }

    #[test]
    fn health_transitions_healthy_to_cooling() {
        let mut h = ModelHealth::new("doubao/x");
        assert!(h.is_callable());
        h.record_failure(
            ErrorKind::Transient,
            "503".into(),
            Instant::now(),
        );
        assert!(matches!(h.status, ModelStatus::Cooling { .. }));
        assert!(!h.is_callable());
        assert_eq!(h.consecutive_failures, 1);
    }

    #[test]
    fn health_balance_disables_immediately() {
        let mut h = ModelHealth::new("doubao/x");
        h.record_failure(ErrorKind::Balance, "402".into(), Instant::now());
        assert!(matches!(h.status, ModelStatus::Disabled { .. }));
    }

    #[test]
    fn health_auth_uses_grace_window() {
        let mut h = ModelHealth::new("doubao/x");
        for _ in 0..AUTH_DISABLE_AFTER - 1 {
            h.record_failure(ErrorKind::Auth, "401".into(), Instant::now());
        }
        // Still Cooling (in grace) — not Disabled yet.
        assert!(matches!(h.status, ModelStatus::Cooling { .. }));
        h.record_failure(ErrorKind::Auth, "401".into(), Instant::now());
        // Now over the threshold.
        assert!(matches!(h.status, ModelStatus::Disabled { .. }));
    }

    #[test]
    fn health_success_resets() {
        let mut h = ModelHealth::new("doubao/x");
        h.record_failure(ErrorKind::Transient, "503".into(), Instant::now());
        h.record_success();
        assert!(matches!(h.status, ModelStatus::Healthy));
        assert_eq!(h.consecutive_failures, 0);
    }

    #[test]
    fn health_reset_clears_disabled() {
        let mut h = ModelHealth::new("doubao/x");
        h.record_failure(ErrorKind::Balance, "402".into(), Instant::now());
        assert!(matches!(h.status, ModelStatus::Disabled { .. }));
        h.reset();
        assert!(matches!(h.status, ModelStatus::Healthy));
    }

    #[test]
    fn chain_next_callable_skips_disabled() {
        let mut chain = ChainHealth::from_chain(["a", "b", "c"]);
        chain
            .get_mut("a")
            .unwrap()
            .record_failure(ErrorKind::Balance, "".into(), Instant::now());
        assert_eq!(chain.next_callable().unwrap().model, "b");
    }

    #[test]
    fn chain_all_unavailable_when_drained() {
        let mut chain = ChainHealth::from_chain(["a", "b"]);
        chain
            .get_mut("a")
            .unwrap()
            .record_failure(ErrorKind::Balance, "".into(), Instant::now());
        chain
            .get_mut("b")
            .unwrap()
            .record_failure(ErrorKind::Balance, "".into(), Instant::now());
        assert!(chain.all_unavailable());
        assert!(chain.next_callable().is_none());
    }

    #[test]
    fn cooling_backoff_caps_at_max() {
        let d = cooling_backoff(20, ErrorKind::RateLimit);
        assert_eq!(d, MAX_COOLDOWN);
    }

    #[test]
    fn cooling_backoff_starts_at_base() {
        assert_eq!(cooling_backoff(1, ErrorKind::RateLimit), Duration::from_secs(30));
        assert_eq!(cooling_backoff(1, ErrorKind::Transient), Duration::from_secs(10));
    }
}
