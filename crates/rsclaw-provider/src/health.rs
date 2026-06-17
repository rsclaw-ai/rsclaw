//! Per-model health tracking for the model-array failover system.
//!
//! Each `ModelConfig` field that supports a chain (primary / flash / vision
//! / image / video) is paired with a `ChainHealth` at runtime. The state
//! machine routes each call through the chain in order, demoting failing
//! models to `Cooling`, which always auto-recovers after a time-bounded
//! backoff.
//!
//! Design choices, locked in with the user:
//! - Status is **runtime-only**; never persisted to rsclaw.json5. The config
//!   stays declarative (`primary: "doubao/x"` or `primary: [a, b]`) and the
//!   user sees what they wrote.
//! - **A runtime failure never permanently disables a model.** Every failure
//!   produces a time-bounded `Cooling`; disabling-class errors (auth /
//!   balance / model-missing) escalate to the `MAX_COOLDOWN` ceiling so a
//!   genuinely-broken config is re-probed at most hourly rather than being
//!   locked out until manual reset. `Disabled` is reserved for explicit
//!   operator/config disable, never reached from call results.
//! - Restart resets all state (no on-disk persistence). Simple and avoids
//!   the redb dance for what's essentially short-term volatile data.

use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

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
    /// 413 `session_ctx_exceeded` from the rsclaw kvCacheMode=2 session
    /// backend: the conversation (system + tools + history) grew past the
    /// worker's `--rsclaw-max-session-ctx`. NOT a model fault and NOT a
    /// failover trigger — switching models just masks it (a bigger-ctx
    /// fallback "works" but slowly). The caller (agent loop) must compact
    /// the history or recreate the session and retry the SAME model.
    /// Propagated, never advances the chain, never disables the model.
    ContextExceeded,
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
    /// Skipped until an explicit reset (CLI / API / config reload that
    /// drops this id) clears it. NOT produced by runtime failures anymore
    /// — `record_failure` always uses a time-bounded `Cooling` so nothing
    /// is permanently locked out. Reserved for explicit/operator disable.
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
    /// Counts consecutive failures. Drives the exponential backoff and,
    /// for disabling-class errors, the escalation to `MAX_COOLDOWN` after
    /// `AUTH_DISABLE_AFTER` strikes. Cleared on success / reset.
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
    ///
    /// A runtime failure NEVER lands in `Disabled` — it always produces a
    /// time-bounded `Cooling` so the model recovers on its own after the
    /// backoff. A transient blip (a flaky download, a brief gateway 5xx,
    /// a key rotated mid-flight) must not lock a model out until a manual
    /// reset. Disabling-class errors (auth / balance / model-missing) still
    /// get the grace window, then escalate to the `MAX_COOLDOWN` ceiling so
    /// a genuinely-broken config is re-probed at most hourly instead of
    /// being hammered every call.
    pub fn record_failure(&mut self, kind: ErrorKind, body_snippet: String, now: Instant) {
        self.last_error = Some(body_snippet);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);

        let backoff = if kind.is_disabling() && self.consecutive_failures >= AUTH_DISABLE_AFTER {
            MAX_COOLDOWN
        } else {
            cooling_backoff(self.consecutive_failures, kind)
        };
        self.status = ModelStatus::Cooling {
            until: now + backoff,
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
// Shared registry — bridges FailoverManager (writer) ↔ HTTP layer (reader)
// ---------------------------------------------------------------------------

/// Cheaply-cloneable wrapper around a shared `ChainHealth` table. One
/// instance is created at gateway startup and cloned into every
/// `FailoverManager` and into `AppState`, so the HTTP layer
/// (`/api/v1/models/health`) sees the same view the failover loops are
/// writing to in real time.
#[derive(Clone, Default)]
pub struct ProviderHealthRegistry {
    inner: Arc<RwLock<ChainHealth>>,
}

impl ProviderHealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensure each model id has a `ModelHealth` entry (Healthy by default).
    /// Called by FailoverManager at the start of each `call` so health
    /// rows track only models the runtime actually attempts.
    pub fn ensure(&self, models: &[String]) {
        let mut g = match self.inner.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        for m in models {
            if g.get_mut(m).is_none() {
                g.entries.push(ModelHealth::new(m.clone()));
            }
        }
    }

    /// Whether the chain may call this model right now (Healthy or
    /// Cooling-expired). Missing model → callable (lazy init in `ensure`).
    pub fn is_callable(&self, model: &str) -> bool {
        let g = match self.inner.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.entries
            .iter()
            .find(|e| e.model == model)
            .map(|e| e.is_callable())
            .unwrap_or(true)
    }

    /// Record a successful call — flips entry to Healthy + clears counters.
    pub fn record_success(&self, model: &str) {
        let mut g = match self.inner.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(h) = g.get_mut(model) {
            h.record_success();
        }
    }

    /// Record a failure — applies the kind's transition (Cooling /
    /// Disabled) and stores the truncated error body for telemetry.
    pub fn record_failure(&self, model: &str, kind: ErrorKind, body: String) {
        let mut g = match self.inner.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(h) = g.get_mut(model) {
            h.record_failure(kind, body, Instant::now());
        }
    }

    /// Manually clear a model's Disabled/Cooling state. Returns true if
    /// the model was in the table, false if unknown.
    pub fn reset(&self, model: &str) -> bool {
        let mut g = match self.inner.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(h) = g.get_mut(model) {
            h.reset();
            true
        } else {
            false
        }
    }

    /// Read-only snapshot for /api/v1/models/health.
    pub fn snapshot(&self) -> Vec<HealthEntrySnapshot> {
        let g = match self.inner.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let now = Instant::now();
        g.entries
            .iter()
            .map(|e| HealthEntrySnapshot {
                model: e.model.clone(),
                status: status_label(&e.status),
                reason: reason_for(&e.status),
                cooldown_seconds: cooldown_seconds(&e.status, now),
                last_error: e.last_error.clone(),
                consecutive_failures: e.consecutive_failures,
            })
            .collect()
    }

    /// Snapshot of every model id known to the table — for diagnostics.
    pub fn model_ids(&self) -> Vec<String> {
        let g = match self.inner.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.entries.iter().map(|e| e.model.clone()).collect()
    }
}

/// Wire-format entry for the `/api/v1/models/health` endpoint. Matches
/// the contract the UI brief specifies (status string, optional reason,
/// optional cooldown_seconds).
#[derive(Debug, Clone, serde::Serialize)]
pub struct HealthEntrySnapshot {
    pub model: String,
    /// "Healthy" | "Cooling" | "Disabled"
    pub status: &'static str,
    /// Disabled only — one of "Balance"/"Auth"/"ModelMissing"/etc. (the
    /// ErrorKind debug label). Null for Healthy/Cooling.
    pub reason: Option<String>,
    /// Cooling only — seconds until the entry becomes callable again.
    /// Null for Healthy/Disabled.
    pub cooldown_seconds: Option<u64>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

fn status_label(s: &ModelStatus) -> &'static str {
    match s {
        ModelStatus::Healthy => "Healthy",
        ModelStatus::Cooling { .. } => "Cooling",
        ModelStatus::Disabled { .. } => "Disabled",
    }
}

fn reason_for(s: &ModelStatus) -> Option<String> {
    match s {
        ModelStatus::Disabled { reason } => Some(reason.clone()),
        _ => None,
    }
}

fn cooldown_seconds(s: &ModelStatus, now: Instant) -> Option<u64> {
    match s {
        ModelStatus::Cooling { until } => {
            if *until > now {
                Some((*until - now).as_secs())
            } else {
                Some(0)
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Disabling-class failures (auth / balance / model-missing) absorbed at
/// the normal exponential backoff before escalating to the `MAX_COOLDOWN`
/// ceiling. 3 = one cached-key race + one operator rotation slip + one real
/// failure. Beyond that the signal is strong enough to back off to hourly
/// re-probes (still bounded — never a permanent lock-out).
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
        // BadRequest / ContextExceeded are caller-handled (propagated, not
        // advanced), so they never actually cool down a profile — this arm
        // only satisfies exhaustiveness.
        ErrorKind::BadRequest | ErrorKind::ContextExceeded => 5u64,
        // Disabling kinds never get here (early-returned in record_failure).
        ErrorKind::Auth | ErrorKind::Balance | ErrorKind::ModelMissing => 60u64,
    };
    // Cap exponent at 16 so MAX_COOLDOWN (1h) is the binding constraint
    // rather than the doubling — 30s × 2^16 ≈ 22d, easily clamped to 1h.
    // A tighter cap would prevent ever reaching MAX_COOLDOWN with the
    // smaller bases.
    let exponent = consecutive.saturating_sub(1).min(16);
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

    // -------- Session context budget exhausted (rsclaw kvCacheMode=2) --
    // The gateway returns HTTP 413 with error.code = "session_ctx_exceeded"
    // when the session's (system + tools + history) tokens exceed the
    // worker's --rsclaw-max-session-ctx. Match this BEFORE the generic
    // max_tokens / "exceed" → BadRequest bucket below, because the body
    // contains both "exceed_context_size_error" and "max-session-ctx" and
    // we want the dedicated kind, not BadRequest. This is caller-handled
    // (compact / recreate the session), NOT a model failover trigger.
    if lower.contains("session_ctx_exceeded")
        || lower.contains("exceed_context_size_error")
        || lower.contains("max-session-ctx")
    {
        return ErrorKind::ContextExceeded;
    }

    // -------- Balance / quota — strongest signal, check first --------
    // Volcengine Ark: "AccountOverdueError" body, sometimes status 402.
    // OpenAI: "insufficient_quota" / "billing".
    // Anthropic: "credit_balance_too_low".
    // Chinese error messages from doubao / qwen: "余额不足".
    if lower.contains("insufficient_quota")
        || lower.contains("insufficient quota")
        || lower.contains("credit_balance_too_low")
        || (lower.contains("credit balance") && lower.contains("too low"))
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
    fn classify_session_ctx_exceeded() {
        // The gateway's 413 envelope (rsclaw kvCacheMode=2 session backend).
        let body = r#"413 Payload Too Large: {"error":{"code":"session_ctx_exceeded","detail":"session has grown past the worker's max-session-ctx budget; call /compact to summarize history or recreate the session"}}"#;
        assert_eq!(classify_str(body), ErrorKind::ContextExceeded);
    }

    #[test]
    fn classify_session_ctx_exceeded_worker_envelope() {
        // The raw worker envelope, in case it reaches the client un-mapped.
        let body = r#"{"error":{"code":400,"message":"session_ctx_exceeded: replay session would start with 35518 tokens, reaching --rsclaw-max-session-ctx=32768; compact history before replaying this session","type":"exceed_context_size_error","n_prompt_tokens":35518,"n_ctx":32768}}"#;
        assert_eq!(classify_str(body), ErrorKind::ContextExceeded);
    }

    #[test]
    fn classify_session_ctx_exceeded_not_confused_with_plain_max_tokens() {
        // A plain max_tokens overage (no session-ctx markers) stays BadRequest,
        // so the dedicated kind doesn't over-capture generic context errors.
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
    fn health_balance_cools_bounded_not_disabled() {
        let mut h = ModelHealth::new("doubao/x");
        h.record_failure(ErrorKind::Balance, "402".into(), Instant::now());
        // Bounded cooldown — never a permanent Disabled from a call result.
        assert!(matches!(h.status, ModelStatus::Cooling { .. }));
        assert!(!h.is_callable());
    }

    #[test]
    fn health_auth_escalates_to_bounded_cooldown_not_disabled() {
        let now = Instant::now();
        let mut h = ModelHealth::new("doubao/x");
        for _ in 0..AUTH_DISABLE_AFTER - 1 {
            h.record_failure(ErrorKind::Auth, "401".into(), now);
        }
        // Still Cooling on the normal backoff (in grace).
        assert!(matches!(h.status, ModelStatus::Cooling { .. }));
        // Crossing the threshold escalates to the MAX_COOLDOWN ceiling, but
        // it's still a (long) bounded Cooling — never Disabled.
        h.record_failure(ErrorKind::Auth, "401".into(), now);
        match h.status {
            ModelStatus::Cooling { until } => assert_eq!(until, now + MAX_COOLDOWN),
            other => panic!("expected bounded Cooling, got {other:?}"),
        }
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
    fn health_reset_clears_cooldown() {
        let mut h = ModelHealth::new("doubao/x");
        h.record_failure(ErrorKind::Balance, "402".into(), Instant::now());
        assert!(matches!(h.status, ModelStatus::Cooling { .. }));
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
