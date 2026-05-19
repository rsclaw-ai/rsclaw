//! rsclaw-server kvCacheMode=2 provider — incremental session protocol.
//!
//! Wire-level contract: see `~/dev/rsclaw-llm/docs/rsclaw-protocol.md` v1.1+.
//!
//! Stateful sessions where rsclaw-server is the source of truth for
//! conversation history. Per-turn the client sends only the delta
//! (new user message OR tool_results); the server's KV cache stays
//! hot across turns.
//!
//! This provider rejects requests with `kv_cache_mode != 2` — those
//! must go through one of the regular OAI / Anthropic providers.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt, future::BoxFuture};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    AgentEndpoint, ContentPart, LlmProvider, LlmRequest, LlmStream, Message, MessageContent, Role,
    StreamEvent, TokenUsage,
};

/// Default base for the rsclaw-server fleet. The `/v1/agent` suffix
/// is the external API mount inside rsclaw-server — the rsclaw-llm
/// `/sessions/...` protocol paths are exposed to clients under that
/// prefix, distinct from `/v1/chat/completions` etc. Setting
/// `RSCLAW_URL` overrides this; that variable should also include the
/// `/v1/agent` segment.
///
/// `api.rsclaw.ai` fronts the fleet behind a 308-emitting LB that
/// pins clients (via the [`RedirectCache`] in this module) to their
/// resolved worker host for the response's `Cache-Control: max-age`
/// window (1h by default). First request through any provider
/// instance pays the redirect cost; everything within the TTL after
/// goes direct, so steady-state latency matches a direct deployment.
pub const RSCLAW_DEFAULT_BASE: &str = "https://api.rsclaw.ai/v1/agent";

/// Default `prefix_id` per protocol §2.1.1 / §2.10.1 — namespaced
/// `<ns>/<ver>` string the gateway sends on `POST /sessions`. It's a
/// STATIC identifier (config-driven, never derived from `req.model` or
/// any hash of the request body) so the worker can route the call to
/// its static-registry prefix when one is registered, and otherwise
/// fall back to the dynamic-LRU keyed by `hash(system + tools)`
/// computed worker-side. `req.model` deliberately does NOT participate
/// in prefix_id construction — model selection is independent from the
/// prefix-cache identity. Override via the per-provider config field
/// `prefix_id` (see `ProviderConfig::prefix_id`).
///
/// The `<ver>` component here is the **baseline** version, NOT the
/// gateway's `CARGO_PKG_VERSION`. They are decoupled on purpose:
/// rsclaw-llm has to manually pre-register a base-layer KV slot for
/// each unique prefix_id, and a typical Cargo bump (patch release,
/// channel hot-fix, debug toggle) does not change the canonical
/// `shared_prefix + builtin_tools` payload that this identifier
/// names. Auto-tracking `CARGO_PKG_VERSION` would invalidate the
/// worker's registered slot on every gateway release. Bump this
/// string by hand only when the canonical baseline (asserted by
/// `tests/fixtures/baseline-<ver>.json`) actually changes, and
/// coordinate with rsclaw-llm to re-ingest the new fixture under
/// the new identifier.
pub const RSCLAW_DEFAULT_PREFIX_ID: &str = "rsclaw/2026.5.18";

/// Well-known model names served by the managed rsclaw fleet (see
/// `GET /v1/agent/models`). Used by `agent::runtime` to auto-resolve
/// `flash` and `vision` when the user only configured a `rsclaw/*`
/// primary model. The contract: as long as your `model.primary` lives
/// under the `rsclaw/` namespace, you get the rsclaw flash and vision
/// slots for free without repeating them in config. Override via
/// `agents.defaults.model.flash` / `agents.defaults.model.vision` when
/// you want a different model.
///
/// Keep these in sync with whatever the fleet ingests under the
/// version named by [`RSCLAW_DEFAULT_PREFIX_ID`]. When the fleet bumps
/// a model name (e.g. `rsclaw-flash-v2`), bump both — clients on the
/// new gateway version pick up the new defaults automatically without
/// every user having to edit their config.
pub const RSCLAW_DEFAULT_FLASH: &str = "rsclaw/rsclaw-flash-v1";
pub const RSCLAW_DEFAULT_VISION: &str = "rsclaw/rsclaw-vision-v1";

/// How long to wait for `/sessions/<id>/turn` to start responding
/// (TCP connect + TLS + send body + receive headers + first byte).
/// Once the body stream begins this deadline no longer applies — the
/// SSE body is allowed to take as long as the model needs.
const TURN_HEADERS_TIMEOUT: Duration = Duration::from_secs(60);

/// Hard cap on the in-memory `sessions` cache. Each entry is a few
/// dozen bytes (`session_id` + `prefix_id` + counter), so 10_000 caps
/// the per-process footprint at ~1MB even under churn. When the cap is
/// hit, [`evict_if_oversized`] drops half the entries — picked by
/// HashMap iteration order, which is good enough since we lack
/// last-access timestamps and the alternative (LRU bookkeeping) would
/// add a synchronisation hot spot. Evicted entries cause one extra
/// replay on their next access, which is the same recovery path used
/// for server-side eviction (§2.2).
///
/// Without this cap a long-running gateway with high session churn
/// (every WeChat user = one session_key) accumulates entries forever
/// and bleeds memory until OOM. Mirrors the pre-existing safeguard in
/// the previous OpenAI-provider implementation that this provider
/// replaced.
const MAX_SESSIONS: usize = 10_000;

/// Maximum redirect hops we'll follow before bailing. 5 is generous —
/// a healthy rsclaw-server topology should be at most ONE 308 hop
/// (LB / API gateway → real worker), but multi-region or canary fleet
/// setups can stack two. Above five we're almost certainly in a loop
/// or fighting a misconfigured DNS round-robin returning 308 forever.
const MAX_REDIRECT_HOPS: usize = 5;

/// Fallback redirect-cache TTL when the 308 response omits a
/// `Cache-Control: max-age=N` directive. We deliberately set this
/// SHORT (5 min) rather than long: a server that forgets to declare
/// its TTL probably has buggy 308 emission too, so we should
/// re-validate often enough that we don't get pinned to a stale
/// target. Servers that want longer caching MUST set `max-age=3600`
/// (or whatever they prefer) explicitly. rsclaw-server's documented
/// default is `max-age=3600` (1h).
const DEFAULT_REDIRECT_TTL: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct RsclawProvider {
    /// Reqwest client with redirects DISABLED — we manage 307/308
    /// ourselves so we can capture the `Cache-Control` directive on
    /// 308 responses (reqwest's auto-redirect strips the response
    /// headers before we can read them). Other providers in this
    /// crate still use the default redirect policy via
    /// `http_client_with_ua` — this is rsclaw-specific because only
    /// rsclaw-server is part of OUR infrastructure (other providers
    /// are arbitrary upstreams where redirect-caching would let an
    /// attacker pin us to a malicious host).
    client: Client,
    base_url: String,
    bearer: Option<String>,
    /// Namespaced `<ns>/<ver>` string sent verbatim as the wire
    /// `prefix_id` on every `POST /sessions` / `POST /sessions/replay`.
    /// Resolved at provider construction from the per-provider config
    /// `prefix_id` field, falling back to [`RSCLAW_DEFAULT_PREFIX_ID`]
    /// when the field is absent. It is intentionally static for the
    /// lifetime of the provider: the worker-side dynamic-LRU is keyed
    /// by `hash(system + tools)` (not `user_system`, not `req.model`),
    /// so threading model or per-request data into this field would
    /// only fragment the cache.
    prefix_id: String,
    sessions: Arc<Mutex<HashMap<String, SessionEntry>>>,
    /// Cache of 308 redirects, keyed by *origin* (scheme + host + port)
    /// of the requested URL. Lets a single LB call amortise across the
    /// whole session (and across sessions, since this is per-provider-
    /// instance and providers are global). See [`RedirectCache`].
    redirect_cache: Arc<Mutex<RedirectCache>>,
}

/// Entry in [`RedirectCache`] — where to actually send requests that
/// target this origin, and until when this rerouting is valid.
#[derive(Debug, Clone)]
struct RedirectEntry {
    /// Scheme + host + port to send to. The path portion of any
    /// request is appended verbatim; e.g. `(api.rsclaw.ai/v1/agent/...
    /// → server.rsclaw.ai:8443/v1/agent/...)`.
    target_origin: String,
    /// Instant after which this entry is ignored. Computed from
    /// `Cache-Control: max-age=N` on the 308 response, or
    /// [`DEFAULT_REDIRECT_TTL`] if the directive was absent.
    expires_at: std::time::Instant,
}

#[derive(Debug, Default)]
struct RedirectCache {
    entries: HashMap<String, RedirectEntry>,
}

impl RedirectCache {
    /// Look up `origin`. Returns the target origin only when the entry
    /// is fresh; expired entries are removed lazily.
    fn lookup(&mut self, origin: &str) -> Option<String> {
        let now = std::time::Instant::now();
        match self.entries.get(origin) {
            Some(e) if e.expires_at > now => Some(e.target_origin.clone()),
            Some(_) => {
                // Lazy eviction — drop the stale entry so subsequent
                // lookups don't keep re-checking the same dead row.
                self.entries.remove(origin);
                None
            }
            None => None,
        }
    }

    fn store(&mut self, origin: String, target_origin: String, ttl: Duration) {
        let expires_at = std::time::Instant::now() + ttl;
        self.entries.insert(origin, RedirectEntry {
            target_origin,
            expires_at,
        });
    }

    fn invalidate(&mut self, origin: &str) {
        self.entries.remove(origin);
    }
}

/// Extract the origin (`scheme://host[:port]`) prefix from a URL.
/// Returns `None` for inputs that don't look like absolute URLs.
fn origin_of(url: &str) -> Option<&str> {
    let scheme_end = url.find("://")? + 3;
    let path_start = url[scheme_end..]
        .find('/')
        .map(|i| scheme_end + i)
        .unwrap_or(url.len());
    Some(&url[..path_start])
}

/// Rewrite the origin portion of `url` to `new_origin`, preserving the
/// path / query / fragment.
fn rewrite_origin(url: &str, new_origin: &str) -> String {
    match origin_of(url) {
        Some(orig) => format!("{}{}", new_origin, &url[orig.len()..]),
        None => url.to_owned(),
    }
}

/// Resolve a possibly-relative `Location` header value against `base`
/// per RFC 3986 §5.3 (minimal — only handles the cases real
/// rsclaw-server emits: absolute URL, absolute path, or empty).
fn resolve_location(base: &str, location: &str) -> Option<String> {
    if location.is_empty() {
        return None;
    }
    if location.contains("://") {
        return Some(location.to_owned());
    }
    let base_origin = origin_of(base)?;
    if location.starts_with('/') {
        Some(format!("{base_origin}{location}"))
    } else {
        // Relative-to-current-path is intentionally not supported.
        // rsclaw-server doesn't emit such Location headers and
        // supporting them would invite path-confusion bugs. Treat as
        // unrecognised and let the caller fall through to error.
        None
    }
}

/// Parse `Cache-Control: ..., max-age=N, ...` returning the number
/// of seconds, or `None` if missing / malformed. Also returns `None`
/// when `no-store` or `no-cache` is present — those directives
/// explicitly forbid caching and override any `max-age` in the same
/// header. Multiple `Cache-Control` headers (rare) are caller's
/// responsibility to concat before calling.
fn parse_max_age(cache_control: Option<&str>) -> Option<Duration> {
    let header = cache_control?;
    let mut max_age: Option<u64> = None;
    for directive in header.split(',') {
        let d = directive.trim();
        let d_lower = d.to_ascii_lowercase();
        if d_lower == "no-store" || d_lower == "no-cache" {
            return None;
        }
        if let Some(value) = d_lower.strip_prefix("max-age=") {
            if let Ok(n) = value.trim().parse::<u64>() {
                max_age = Some(n);
            }
        }
    }
    max_age.map(Duration::from_secs)
}

#[derive(Clone, Debug)]
struct SessionEntry {
    /// Server-issued, format `rs_<instance>_<random>`.
    session_id: String,
    /// `prefix_id` (post-rename, was `rsclaw_version` pre-spec-v1.7) this
    /// session was opened against. A bump triggers re-open since prefix
    /// cache layout changes invalidate the session.
    prefix_id: String,
    /// Largest `req.messages.len()` we've observed on this session.
    /// A subsequent call with a smaller list means the runtime trimmed
    /// history (compaction, repair, reset) and the server-side KV no
    /// longer matches what the gateway thinks the conversation is —
    /// trigger a re-hydrate via /sessions/replay.
    last_seen_msgs_len: usize,
}

impl RsclawProvider {
    pub fn new(base_url: impl Into<String>, bearer: Option<String>) -> Self {
        Self::with_user_agent(base_url, bearer, None)
    }

    /// Create a provider with custom User-Agent.
    pub fn with_user_agent(
        base_url: impl Into<String>,
        bearer: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        // Trim whitespace before trimming trailing slashes — env vars
        // loaded from a dotenv file frequently carry a trailing newline
        // (`RSCLAW_KEY=sk-abc\n`), and reqwest rejects header values
        // containing `\n` outright (RFC 7230 forbids CTLs in field
        // values). Without this, every signed request 500s with
        // "invalid HTTP header value" from inside the client builder
        // before it ever leaves the process. Same hazard applies to
        // base_url where stray whitespace flips reqwest into
        // url-parse-error territory.
        let base_url = base_url
            .into()
            .trim()
            .trim_end_matches('/')
            .to_string();
        let bearer = bearer
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());
        // Build a reqwest client with redirects DISABLED so the
        // 307/308 capture loop in `send_following_redirects` can read
        // each redirect response's `Cache-Control` header before
        // following. reqwest's default policy follows transparently
        // and discards intermediate headers, which would defeat the
        // 308 TTL caching the LB-aware routing depends on. The other
        // tuning (UA, connect timeout, keep-alive, idle pool window)
        // mirrors `http_client_with_ua` since rsclaw-server lives in
        // the same operational envelope as the OAI-compat upstreams
        // that helper was tuned for.
        let client = reqwest::Client::builder()
            .user_agent(user_agent.as_deref().unwrap_or(super::DEFAULT_USER_AGENT))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(10))
            .tcp_keepalive(Duration::from_secs(30))
            .build()
            .expect("failed to build rsclaw HTTP client");
        Self {
            client,
            base_url,
            bearer,
            prefix_id: RSCLAW_DEFAULT_PREFIX_ID.to_owned(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            redirect_cache: Arc::new(Mutex::new(RedirectCache::default())),
        }
    }

    /// Override the default `prefix_id` sent on the wire. Used by the
    /// provider builder in `gateway::providers` when the config carries
    /// an explicit `prefix_id`. Trims whitespace and ignores empty
    /// strings (dotenv-style trailing newlines or unset config keys
    /// would otherwise produce an invalid wire value).
    ///
    /// Also validates the §2.10.1 contract: exactly one `/` separator.
    /// Inputs like `"rsclaw-2026.5.15"` (zero slashes) or
    /// `"foo/bar/baz"` (two slashes) are rejected at the builder so a
    /// typo in config doesn't survive gateway boot and only surface as
    /// a per-session 400 from the server. Rejected inputs are logged at
    /// `warn` and the override is dropped (falls back to the default).
    pub fn with_prefix_id(mut self, prefix_id: impl Into<String>) -> Self {
        let s = prefix_id.into().trim().to_owned();
        if s.is_empty() {
            return self;
        }
        let slash_count = s.matches('/').count();
        if slash_count != 1 {
            tracing::warn!(
                requested_prefix_id = %s,
                slash_count,
                default_prefix_id = RSCLAW_DEFAULT_PREFIX_ID,
                "rsclaw with_prefix_id: ignoring override that violates §2.10.1 \
                 (need exactly one '/' separator); falling back to default"
            );
            return self;
        }
        self.prefix_id = s;
        self
    }

    fn auth_header(&self) -> Option<(String, String)> {
        // `Some("")` slips in when `RSCLAW_KEY` is set but blank (env
        // var present, value empty) — `std::env::var` returns `Ok("")`,
        // gateway/providers.rs `.ok()`s that into `Some("")`. Sending
        // `Authorization: Bearer ` with an empty token gets rejected
        // by stricter proxies and obscures the real "no auth
        // configured" error, so treat empty as absent here.
        self.bearer
            .as_ref()
            .filter(|k| !k.is_empty())
            .map(|k| ("authorization".to_string(), format!("Bearer {k}")))
    }

    /// Acquire the sessions lock, recovering from poison rather than
    /// silently dropping the call.
    ///
    /// `Mutex::lock()` returns `Err` only after a panic occurred while
    /// some other thread held the lock. The pre-poison helpers used
    /// `.ok()?` / `if let Ok(...)`, which silently turned every
    /// post-poison call into a no-op — the provider went brain-dead
    /// (lookups always missed, store/forget became unobservable
    /// drops) but emitted no signal, so operators couldn't tell from
    /// logs that anything was wrong. Recovering with `into_inner()` on
    /// poison preserves the data (HashMap state is itself well-defined
    /// — only an in-flight insert/remove could leave logical staleness,
    /// and that staleness is bounded by the same eviction signals that
    /// already drive replay) and lets us flag the post-mortem in logs.
    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionEntry>> {
        match self.sessions.lock() {
            Ok(g) => g,
            Err(p) => {
                // Use a static `OnceLock` to log only once per process
                // lifetime — poison is a permanent condition, no need
                // to spam every subsequent call.
                use std::sync::OnceLock;
                static LOGGED: OnceLock<()> = OnceLock::new();
                if LOGGED.set(()).is_ok() {
                    tracing::error!(
                        "rsclaw: sessions mutex poisoned — a prior thread \
                         panicked while holding it. Recovering inner data \
                         and continuing; expect possible session-state \
                         drift until restart."
                    );
                }
                p.into_inner()
            }
        }
    }

    /// Atomically look up a cached session AND validate its freshness.
    /// Returns `None` (forcing a re-hydrate) when the entry is missing,
    /// has a stale `prefix_id`, or its `last_seen_msgs_len` exceeds the
    /// incoming `msgs_len` (history was trimmed under our feet). On
    /// success bumps `last_seen_msgs_len` to the new value so the next
    /// call's comparison is against the most recent state.
    fn lookup_and_bump(
        &self,
        session_key: &str,
        prefix_id: &str,
        msgs_len: usize,
    ) -> Option<SessionEntry> {
        let mut map = self.lock_sessions();
        let entry = map.get_mut(session_key)?;
        if entry.prefix_id != prefix_id {
            return None;
        }
        if msgs_len < entry.last_seen_msgs_len {
            return None;
        }
        entry.last_seen_msgs_len = msgs_len;
        Some(entry.clone())
    }

    fn store(&self, session_key: &str, entry: SessionEntry) {
        let mut map = self.lock_sessions();
        map.insert(session_key.to_string(), entry);
        // Cap memory after every insert. Done inline (not on a timer
        // or a separate task) so a sudden churn burst can't tip over
        // the high-water mark while waiting for an external sweeper.
        evict_if_oversized(&mut map);
    }

    fn forget(&self, session_key: &str) {
        let mut map = self.lock_sessions();
        map.remove(session_key);
    }
}

/// Evict roughly half the entries when the cache exceeds [`MAX_SESSIONS`].
/// Iteration order on a `HashMap` is non-deterministic but stable
/// enough within one call to give a consistent set of victims; the
/// alternative (true LRU) would need an auxiliary data structure and a
/// per-call timestamp update on the read path. Evicted sessions cost
/// one extra replay round-trip the next time they're touched — the
/// same code path that handles upstream-side eviction.
fn evict_if_oversized(map: &mut HashMap<String, SessionEntry>) {
    if map.len() <= MAX_SESSIONS {
        return;
    }
    let target_drop = map.len() - MAX_SESSIONS / 2;
    let victims: Vec<String> = map.keys().take(target_drop).cloned().collect();
    let dropped = victims.len();
    for k in victims {
        map.remove(&k);
    }
    tracing::info!(
        cap = MAX_SESSIONS,
        dropped,
        remaining = map.len(),
        "rsclaw: sessions cache over cap, evicted batch"
    );
}

// ---------------------------------------------------------------------------
// LlmProvider impl
// ---------------------------------------------------------------------------

/// Outcome of [`dispatch_decision`] — either a stateless one-shot route
/// or a sentinel telling the caller to continue down the stateful
/// `/v1/agent/sessions/*` protocol path.
#[derive(Debug, PartialEq, Eq)]
enum DispatchRoute {
    OneShot(&'static str),
    Sessions,
}

/// Pure routing classification for an `LlmRequest` on the rsclaw provider.
///
/// Single source of truth: this is what `stream()` consults and what the
/// test suite asserts against. Both bail conditions live here so callers
/// can't silently misroute by forgetting the safety checks.
///
/// Server enforces per-route model whitelists (400 model_slot_mismatch on
/// violations), so canonical model names take priority over endpoint
/// variants. The endpoint variant only matters when the model name is
/// non-canonical.
///
/// Precedence:
///   1. model rsclaw-flash-*                            → /fastshot
///   2. model rsclaw-vision-*                           → /vision
///   3. model rsclaw-agent-* + session_key=None         → /oneshot
///   4. model rsclaw-agent-* + session_key=Some         → /sessions (kvCacheMode=2 required)
///   5. non-canonical model + endpoint=Flash            → /fastshot (server may 400)
///   6. non-canonical model + endpoint=Vision           → /vision (server may 400)
///   7. Primary + session_key=Some                      → /sessions (kvCacheMode=2 required)
///   8. Primary + session_key=None                      → /oneshot
///
/// Bails (before any rule fires):
///   • kv_cache_mode=2 + session_key=None — caller asked for stateful
///     traffic but forgot the session key; would silently drop kvCache.
///   • session_key=Some + kv_cache_mode!=2 — sessions path requires
///     mode 2.
///
/// Trailing-dash on prefixes prevents collisions with hypothetical names
/// like `rsclaw-flashy`.
fn dispatch_decision(req: &LlmRequest) -> Result<DispatchRoute> {
    let bare_model = req.model.strip_prefix("rsclaw/").unwrap_or(&req.model);
    let is_flash_model = bare_model.starts_with("rsclaw-flash-");
    let is_vision_model = bare_model.starts_with("rsclaw-vision-");
    let is_agent_model = bare_model.starts_with("rsclaw-agent-");

    // Safety net: kv_cache_mode=2 requires session_key. Catching this
    // BEFORE the rule chain prevents a stateless misroute (rule 8) from
    // silently dropping kvCache continuity.
    if req.kv_cache_mode == 2 && req.session_key.is_none() {
        anyhow::bail!(
            "rsclaw kv_cache_mode=2 requires session_key (got None); \
             set session_key=Some(...) for stateful traffic or \
             kv_cache_mode=0 + session_key=None for /oneshot"
        );
    }

    // Rules 1–3.
    if is_flash_model {
        return Ok(DispatchRoute::OneShot("/fastshot"));
    }
    if is_vision_model {
        return Ok(DispatchRoute::OneShot("/vision"));
    }
    if is_agent_model && req.session_key.is_none() {
        return Ok(DispatchRoute::OneShot("/oneshot"));
    }

    // Surface an "agent-* model overrides your endpoint hint" warning
    // so operators can debug "I asked for Flash but the request went
    // to /sessions". The canonical model wins by design, but silently
    // (R1 review I1) is debug-hostile in a 1000-worker fleet.
    if is_agent_model && !matches!(req.endpoint, AgentEndpoint::Primary) {
        tracing::warn!(
            model = %req.model,
            endpoint = ?req.endpoint,
            "rsclaw dispatch: agent-* model overrides endpoint hint; routing to /sessions"
        );
    }

    // Rules 5–6: non-canonical model honors endpoint variant hint.
    if !is_agent_model {
        if matches!(req.endpoint, AgentEndpoint::Flash) {
            return Ok(DispatchRoute::OneShot("/fastshot"));
        }
        if matches!(req.endpoint, AgentEndpoint::Vision) {
            return Ok(DispatchRoute::OneShot("/vision"));
        }
    }

    // Rule 8.
    if req.session_key.is_none() {
        return Ok(DispatchRoute::OneShot("/oneshot"));
    }

    // Rules 4 / 7: stateful sessions path requires kv_cache_mode=2.
    if req.kv_cache_mode != 2 {
        anyhow::bail!(
            "rsclaw session-mode call requires kv_cache_mode=2 (got {}); \
             pass session_key=None to route to /oneshot instead",
            req.kv_cache_mode
        );
    }
    Ok(DispatchRoute::Sessions)
}

impl LlmProvider for RsclawProvider {
    fn name(&self) -> &str {
        "rsclaw"
    }

    fn stream(&self, mut req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        Box::pin(async move {
            // Single source of truth for dispatch routing — see
            // [`dispatch_decision`] for the full precedence table.
            match dispatch_decision(&req)? {
                DispatchRoute::OneShot(path) => return self.stream_oneshot(req, path).await,
                DispatchRoute::Sessions => { /* fall through to stateful path */ }
            }
            let session_key = req
                .session_key
                .clone()
                .context("rsclaw kv_cache_mode=2 requires session_key on the request")?;

            // The runtime appends `Role::System` messages AFTER the
            // User/Tool delta on the first iteration of any turn that
            // has dynamic /ctx or just-installed-skill blocks (see
            // agent/runtime.rs ~4068-4082). Without this, `from_request`
            // sees `Role::System` as the last message and aborts the
            // entire turn. Fold trailing System text back into the
            // preceding User delta so the model still gets the context.
            normalize_trailing_system(&mut req.messages);

            let split = split_request(&req, &self.prefix_id)?;

            // Lookup or hydrate. Cache miss / mutation happens on first
            // call, version drift, after a prior replay failure, or
            // after the runtime trimmed history (compaction, repair,
            // reset) — all cases where `req.messages` may not match
            // what the server has hydrated. open() can't hydrate, so
            // when history exists we go straight to replay; an empty
            // history list takes the cheaper open() path.
            let entry = match self.lookup_and_bump(
                &session_key,
                &split.prefix_id,
                req.messages.len(),
            ) {
                Some(e) => e,
                None => {
                    self.forget(&session_key);
                    let history = history_for_replay(&req.messages);
                    let resp = if history.is_empty() {
                        self.open(&split).await?
                    } else {
                        self.replay(&split, history).await?
                    };
                    let entry = SessionEntry {
                        session_id: resp.session_id.clone(),
                        // Cache key MUST be the request value, not the
                        // upstream canonical. open()'s response echoes
                        // the resolved prefix_id (per §2.1.6), which can
                        // differ from the requested alias — e.g. request
                        // `rsclaw/latest`, response `rsclaw/2026.5.15`.
                        // `lookup_and_bump` compares the cached value
                        // against the next call's `split.prefix_id` (also
                        // the alias), so caching the canonical
                        // guarantees a miss on every subsequent call:
                        // re-hydrate every turn, defeating kvCacheMode=2
                        // entirely. Replay's response per §2.2 omits the
                        // field, which happened to make recovery-path
                        // entries self-consistent — but open()-path
                        // entries were always broken. Version drift is
                        // detected server-side via 409 (handled by
                        // is_session_evicted), so we don't need the
                        // canonical here for freshness.
                        prefix_id: split.prefix_id.clone(),
                        last_seen_msgs_len: req.messages.len(),
                    };
                    self.store(&session_key, entry.clone());
                    entry
                }
            };

            let delta = TurnDelta::from_request(&req)?;

            // Optional debug dump — when RSCLAW_DUMP_TURN env is set we
            // write the full request shape (LlmRequest + rsclaw turn body
            // + rsclaw replay body + equivalent OpenAI chat-completions
            // body) to `<base_dir>/debug/turn-<ms>-<sess>.json`. Lets
            // operators replay the SAME turn against different worker
            // endpoints (rsclaw `/sessions/<id>/turn`, `/sessions/replay`,
            // vanilla `/v1/chat/completions`) to bisect protocol-vs-model
            // truncation behavior. No-op when the env var is unset, so
            // production stays untouched.
            if std::env::var("RSCLAW_DUMP_TURN").is_ok() {
                dump_turn_for_debug(&session_key, &entry, &split, &delta, &req);
            }

            // Forget the cached session entry on any non-recoverable
            // turn failure. Without this, an Err here leaves the
            // SessionEntry in cache with a `last_seen_msgs_len` that
            // already counts the delta we tried to send. Failover
            // routes the user-facing turn through another provider,
            // the runtime stores that provider's assistant in session,
            // and on the next rsclaw call `lookup_and_bump` happily
            // returns the stale entry — `turn()` then sends only the
            // *next* delta, so the assistant generated by the fallback
            // never reaches rsclaw-server. Server-side history
            // diverges silently from the runtime's mental model;
            // subsequent generations base their reasoning on a partial
            // log. Forgetting forces a full /sessions/replay on the
            // next rsclaw call, which re-anchors server state to the
            // runtime's complete history.
            let resp = match self.turn(&entry.session_id, &delta, &req).await {
                Ok(o) => o,
                Err(e) => {
                    self.forget(&session_key);
                    return Err(e);
                }
            };
            let resp = match resp {
                TurnOutcome::Stream(s) => s,
                TurnOutcome::SessionNotFound => {
                    // Recover via /sessions/replay then retry the turn.
                    // History excludes the trailing delta — turn() below
                    // re-sends it. Including it in replay would hydrate
                    // the same message twice (once batched, once as the
                    // turn input) and confuse the model.
                    self.forget(&session_key);
                    let replay_history = history_for_replay(&req.messages);
                    let replayed = self.replay(&split, replay_history).await?;
                    let entry = SessionEntry {
                        session_id: replayed.session_id.clone(),
                        // Same rationale as the open/replay path above:
                        // cache key is the request alias, not the
                        // upstream canonical. (Replay's response per
                        // §2.2 doesn't even include prefix_id, so this
                        // site happened to be self-consistent before —
                        // but normalising both sites on
                        // `split.prefix_id` keeps the cache-key contract
                        // single-sourced.)
                        prefix_id: split.prefix_id.clone(),
                        last_seen_msgs_len: req.messages.len(),
                    };
                    self.store(&session_key, entry.clone());
                    // Same forget-on-Err treatment as the primary
                    // turn() path above — recovery doesn't grant
                    // immunity from divergence.
                    match self.turn(&entry.session_id, &delta, &req).await {
                        Ok(TurnOutcome::Stream(s)) => s,
                        Ok(TurnOutcome::SessionNotFound) => {
                            self.forget(&session_key);
                            anyhow::bail!(
                                "rsclaw: session vanished immediately after replay (id={})",
                                entry.session_id
                            );
                        }
                        Err(e) => {
                            self.forget(&session_key);
                            return Err(e);
                        }
                    }
                }
            };
            // Wrap the stream so that the FIRST transport error or
            // explicit `StreamEvent::Error` evicts the session entry.
            // If the stream tears down mid-turn, the runtime sees the
            // error and aborts the iteration — but the rsclaw provider
            // would otherwise keep the session cached, and rsclaw-
            // server's view of that turn could be partially-committed
            // or rolled back depending on where the failure landed.
            // Forcing a fresh replay on the next call re-anchors both
            // sides to the runtime's confirmed history.
            Ok(invalidate_on_error(
                resp,
                Arc::clone(&self.sessions),
                session_key,
            ))
        })
    }

    /// Resolve `session_key` → wire `session_id` via the cached
    /// `SessionEntry`, then delegate to the inner `compact_splice`
    /// helper. On HTTP success, update the cached entry's
    /// `last_seen_msgs_len` to the post-splice value so subsequent
    /// `lookup_and_bump` calls don't (incorrectly) detect the message
    /// drop as "history was trimmed under us" and force a replay.
    ///
    /// Per the 2026-05-16 decision (Listen-first), the cached
    /// `last_seen_msgs_len` is updated from the gateway-local
    /// computation (`keep_head_messages + 1 + keep_tail_messages`) NOT
    /// from `resp.msgs_count`. The server-reported `msgs_count` is
    /// returned to the caller for cross-check / telemetry only.
    ///
    /// Returns `Err` when no `SessionEntry` exists for `session_key`
    /// (no point splicing what we don't think is open) — caller falls
    /// back to the replay path which will re-open the session anyway.
    fn compact_splice<'a>(
        &'a self,
        session_key: &'a str,
        keep_head_messages: usize,
        summary: &'a str,
        keep_tail_messages: usize,
        expected_msgs_count: Option<usize>,
    ) -> BoxFuture<'a, Result<usize>> {
        Box::pin(async move {
            // Snapshot the session_id under the lock, drop the lock
            // before the network call. Holding the mutex across an
            // await would block every other turn on this provider for
            // the duration of the splice.
            let session_id = {
                let map = self.lock_sessions();
                map.get(session_key)
                    .map(|e| e.session_id.clone())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "rsclaw compact splice: no cached session for key {session_key} — \
                             falling back to replay"
                        )
                    })?
            };

            let resp = self
                .compact_splice_inner(
                    &session_id,
                    keep_head_messages,
                    summary,
                    keep_tail_messages,
                    expected_msgs_count,
                )
                .await?;

            // Optimistically update last_seen_msgs_len with the
            // gateway-local computation (head + summary(1) + tail).
            // The server's resp.msgs_count is also tracked but kept as
            // a sanity cross-check at log level.
            let local_msgs_count = keep_head_messages + 1 + keep_tail_messages;
            if resp.msgs_count != local_msgs_count {
                tracing::warn!(
                    session_key,
                    server_count = resp.msgs_count,
                    local_count = local_msgs_count,
                    "rsclaw compact splice: server msgs_count diverges from gateway computation"
                );
            }
            // Emit the server's authoritative post-splice counters here
            // (rather than at the call site in `compact_inner`) because
            // tokens_count is exposed only by the wire response — the
            // trait surface itself returns msgs_count alone. Telemetry
            // tools that need to track KV slot tokens over time should
            // scrape this log.
            tracing::info!(
                session_key,
                msgs_count = resp.msgs_count,
                tokens_count = resp.tokens_count,
                "rsclaw compact splice: server-side splice complete"
            );
            {
                let mut map = self.lock_sessions();
                if let Some(entry) = map.get_mut(session_key) {
                    entry.last_seen_msgs_len = local_msgs_count;
                }
            }
            Ok(resp.msgs_count)
        })
    }
}

/// Wrap an `LlmStream` so the first error item evicts `session_key`
/// from the shared session cache. `errored` flips on the first
/// error to make the eviction idempotent — multiple `Err` items in
/// the same stream don't try to re-acquire the lock unnecessarily.
fn invalidate_on_error(
    inner: LlmStream,
    sessions: Arc<Mutex<HashMap<String, SessionEntry>>>,
    session_key: String,
) -> LlmStream {
    use futures::StreamExt;
    let mut errored = false;
    Box::pin(inner.inspect(move |item| {
        if errored {
            return;
        }
        let invalidate = match item {
            Err(_) => true,
            Ok(StreamEvent::Error(_)) => true,
            _ => false,
        };
        if !invalidate {
            return;
        }
        errored = true;
        // Best-effort lock; if poisoned, the parent provider's
        // `lock_sessions` already logged the original poison —
        // don't compound the noise here.
        match sessions.lock() {
            Ok(mut map) => {
                map.remove(&session_key);
            }
            Err(p) => {
                p.into_inner().remove(&session_key);
            }
        }
    }))
}

// ---------------------------------------------------------------------------
// Protocol operations: open / turn / replay (internal)
// ---------------------------------------------------------------------------

impl RsclawProvider {
    /// Resolve a request URL via the redirect cache.
    ///
    /// Strips the configured `base_url`'s origin off the front, looks
    /// the origin up in [`RedirectCache`], and if there's a fresh 308
    /// cached, rewrites the URL to point at the cached target instead.
    /// This is what lets the very first request through the LB pay
    /// the redirect cost ONCE, then route directly for the cache TTL
    /// (1h by rsclaw-server's default).
    fn resolve_url(&self, path: &str) -> String {
        let full = format!("{}{}", self.base_url, path);
        let Some(origin) = origin_of(&full) else {
            return full;
        };
        if let Ok(mut cache) = self.redirect_cache.lock() {
            if let Some(target) = cache.lookup(origin) {
                return rewrite_origin(&full, &target);
            }
        }
        full
    }

    /// Invalidate any cached redirect entry whose origin matches `url`.
    /// Called by request paths that observe target-host failure
    /// (connection refused, 5xx, timeout) so a dead target doesn't
    /// keep getting hammered for the rest of the cache window.
    fn invalidate_redirect_for(&self, url: &str) {
        let Some(origin) = origin_of(url) else { return };
        if let Ok(mut cache) = self.redirect_cache.lock() {
            cache.invalidate(origin);
        }
    }

    /// POST `body` to `path` (e.g. `/sessions`) under `base_url`,
    /// transparently capturing 307/308 redirects.
    ///
    /// - 308 responses: extract `Location` + `Cache-Control: max-age`,
    ///   record the target origin in [`RedirectCache`] with that TTL
    ///   (or [`DEFAULT_REDIRECT_TTL`] if absent), then follow once.
    ///   Subsequent calls through [`resolve_url`] go DIRECT to the
    ///   target until the TTL expires — amortising the LB cost across
    ///   the whole cache window instead of paying it per request.
    /// - 307 responses: follow without caching (temporary by spec).
    /// - All other statuses: returned as-is to the caller, even
    ///   error statuses. Callers decide how to interpret them
    ///   (e.g. turn() recognises 404/409/503 as session-evicted and
    ///   triggers replay).
    ///
    /// `body_max_age_fallback` is the per-request override for the
    /// "no Cache-Control on 308" fallback TTL. Passing `None` uses
    /// [`DEFAULT_REDIRECT_TTL`]. Currently always `None`, but the
    /// hook exists for callers that want a different policy.
    ///
    /// `builder_timeout` controls reqwest's total request deadline
    /// per individual hop (NOT cumulative across redirects). `None`
    /// = no builder timeout, used by streaming `turn()` which wraps
    /// the headers phase externally with `tokio::time::timeout`.
    async fn send_following_redirects<B: Serialize>(
        &self,
        path: &str,
        body: &B,
        builder_timeout: Option<Duration>,
    ) -> Result<reqwest::Response> {
        let mut current_url = self.resolve_url(path);
        let mut hops = 0;

        loop {
            let mut builder = self.client.post(&current_url).json(body);
            if let Some(t) = builder_timeout {
                builder = builder.timeout(t);
            }
            if let Some((k, v)) = self.auth_header() {
                builder = builder.header(k, v);
            }
            let resp = match super::send_with_transport_retry(builder).await {
                Ok(r) => r,
                Err(e) => {
                    // Transport-level failure against a redirected
                    // target → invalidate so the next attempt goes
                    // back through the LB. Connection refused / DNS
                    // failure on the cached target is the canonical
                    // "target host is dead, LB should reroute" signal.
                    self.invalidate_redirect_for(&current_url);
                    return Err(anyhow::Error::from(e)
                        .context(format!("rsclaw POST {current_url}")));
                }
            };

            let status = resp.status();
            if status != StatusCode::TEMPORARY_REDIRECT
                && status != StatusCode::PERMANENT_REDIRECT
            {
                return Ok(resp);
            }

            // Redirect path. Pull Location + Cache-Control before
            // consuming the response.
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let cache_control = resp
                .headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);

            let Some(loc) = location else {
                anyhow::bail!(
                    "rsclaw: {status} redirect from {current_url} omitted Location header"
                );
            };
            let Some(next_url) = resolve_location(&current_url, &loc) else {
                anyhow::bail!(
                    "rsclaw: {status} redirect from {current_url} had unsupported Location {loc:?}"
                );
            };

            // 308 only: cache the target origin so future requests
            // skip the LB. 307 is temporary by spec and explicitly
            // must NOT be cached (otherwise we'd defeat the whole
            // point of the LB using 307 for ephemeral moves).
            if status == StatusCode::PERMANENT_REDIRECT {
                if let (Some(current_origin), Some(next_origin)) =
                    (origin_of(&current_url), origin_of(&next_url))
                {
                    // Only cache when the origin actually changed —
                    // a 308 that points back at the same origin is a
                    // server misconfiguration (would cause an infinite
                    // loop), surface it via the hop counter below
                    // rather than caching the self-loop.
                    if current_origin != next_origin {
                        let ttl = parse_max_age(cache_control.as_deref())
                            .unwrap_or(DEFAULT_REDIRECT_TTL);
                        if let Ok(mut cache) = self.redirect_cache.lock() {
                            cache.store(
                                current_origin.to_owned(),
                                next_origin.to_owned(),
                                ttl,
                            );
                        }
                        tracing::info!(
                            from = %current_origin,
                            to = %next_origin,
                            ttl_secs = ttl.as_secs(),
                            "rsclaw: cached 308 redirect"
                        );
                    }
                }
            }

            hops += 1;
            if hops > MAX_REDIRECT_HOPS {
                anyhow::bail!(
                    "rsclaw: too many redirects ({MAX_REDIRECT_HOPS} hops) starting from {path}"
                );
            }
            current_url = next_url;
        }
    }

    async fn open(&self, split: &SplitRequest<'_>) -> Result<CreateSessionResp> {
        let body = CreateSessionReq {
            prefix_id: &split.prefix_id,
            model: &split.model,
            dynamic_prefix: DynamicPrefixWire {
                system: split.dynamic_system,
                tools: &split.dynamic_tools,
                user_system: split.dynamic_user_system,
            },
            options: Some(split.options.clone()),
        };
        // 180s caps the worst-case prefix-decode time for a fresh
        // session; without an explicit timeout reqwest hangs forever
        // on a stalled server (the 20s connect_timeout only covers TCP
        // establishment, not response wait). The deadline is per-hop
        // so a redirected open still gets the full budget against the
        // ultimate target rather than splitting it.
        let resp = self
            .send_following_redirects("/sessions", &body, Some(Duration::from_secs(180)))
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("rsclaw open session failed {status}: {body}");
        }
        resp.json::<CreateSessionResp>()
            .await
            .context("rsclaw open: parse response")
    }

    async fn replay(
        &self,
        split: &SplitRequest<'_>,
        messages: &[Message],
    ) -> Result<CreateSessionResp> {
        // Protocol §2.2 history accepts only `role: "user"` and
        // `role: "assistant"`. The runtime, however, threads
        // `Role::System` messages into the conversation list for
        // plugins/skills prefixes, just-installed skills, and
        // dynamic /ctx blocks (see agent/runtime.rs ~4054). Sending
        // those through as-is would trigger `400 invalid_history`
        // and tank every replay. Pull them out and append their
        // text to `user_system` so the content still reaches the
        // server — at the static-prefix slot, the only place the
        // protocol allows non-conversational system content.
        let (filtered, extra_suffix) = split_system_messages(messages);
        // System-Role messages threaded through the conversation list
        // (plugins/skills/ctx blocks — see comment above) get folded
        // back into `dynamic_prefix.user_system`, the only protocol
        // slot that accepts non-conversational system content. Without
        // this they'd hit `400 invalid_history` on the worker side.
        let user_system_owned: String = if extra_suffix.is_empty() {
            String::new()
        } else if split.dynamic_user_system.is_empty() {
            extra_suffix
        } else {
            format!("{}\n\n{}", split.dynamic_user_system, extra_suffix)
        };
        let user_system: &str = if user_system_owned.is_empty() {
            split.dynamic_user_system
        } else {
            &user_system_owned
        };
        let history: Vec<Value> = serialize_replay_history(&filtered);
        let body = ReplayReq {
            prefix_id: &split.prefix_id,
            model: &split.model,
            dynamic_prefix: DynamicPrefixWire {
                system: split.dynamic_system,
                tools: &split.dynamic_tools,
                user_system,
            },
            history,
            options: Some(split.options.clone()),
        };
        // 300s — replay re-decodes prefix + full history, which is
        // strictly slower than open()'s prefix-only decode (180s).
        // Without an explicit timeout reqwest hangs forever on a
        // stalled server (connect_timeout only covers TCP setup).
        // Deadline applies per redirect hop so a redirected replay
        // still gets the full budget against the ultimate target.
        let resp = self
            .send_following_redirects(
                "/sessions/replay",
                &body,
                Some(Duration::from_secs(300)),
            )
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("rsclaw replay failed {status}: {body}");
        }
        resp.json::<CreateSessionResp>()
            .await
            .context("rsclaw replay: parse response")
    }

    /// In-place compact splice (protocol §2.4). Issues
    /// `POST /v1/agent/sessions/<session_id>/compact` to ask the server to
    /// drop the KV pages for the middle of the conversation, prefill the
    /// new summary in their place, and leave head/tail KV unchanged. The
    /// session's KV slot — and therefore its `session_id` — survives.
    ///
    /// Caller responsibilities:
    /// - Provide `session_id` from the cached `SessionEntry`. Server
    ///   returns 410 if the slot has been evicted.
    /// - Choose `keep_head_messages` consistently across the lifetime of
    ///   a session (typically 2 = first user/assistant pair carrying
    ///   `[Session started: ...]`). Changing it mid-session breaks the
    ///   head-byte-stability invariant and forces a head re-prefill on
    ///   the server.
    /// - Provide a self-contained `summary` (no `[Session started:]` —
    ///   that's preserved in head — but a fresh `[CONTEXT COMPACTION
    ///   compacted at <ISO ts>]` header is the convention so the model
    ///   has a "recent-vs-summarized" temporal anchor; this struct does
    ///   not enforce that format).
    /// - On any `Err`, callers MUST fall back to `/sessions/replay` —
    ///   `compact_inner` does this unconditionally (per user 2026-05-16
    ///   decision). 409 / 410 / 422 are the documented fallback codes but
    ///   the contract is "any non-2xx + any transport error → replay".
    ///
    /// Timeout: 180s. The server-side splice involves dropping KV pages
    /// and prefilling the summary (~2K tokens by default), which is
    /// fast — comparable to a small turn prefill. The deadline matches
    /// `open()` so we don't have an inconsistent ceiling between the
    /// two new-KV-content code paths.
    ///
    /// Named with the `_inner` suffix to disambiguate from the
    /// `LlmProvider::compact_splice` trait method, which sits one layer
    /// above and resolves `session_key` → `session_id` before delegating
    /// here. The trait method is the public API; this is the wire-level
    /// implementation.
    async fn compact_splice_inner(
        &self,
        session_id: &str,
        keep_head_messages: usize,
        summary: &str,
        keep_tail_messages: usize,
        expected_msgs_count: Option<usize>,
    ) -> Result<CompactSpliceResp> {
        let path = format!("/sessions/{}/compact", session_id);
        let body = CompactSpliceReq {
            keep_head_messages,
            summary,
            keep_tail_messages,
            expected_msgs_count,
        };
        let resp = self
            .send_following_redirects(&path, &body, Some(Duration::from_secs(180)))
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("rsclaw compact splice failed {status}: {body}");
        }
        resp.json::<CompactSpliceResp>()
            .await
            .context("rsclaw compact: parse response")
    }

    /// Dispatch a one-shot stateless request to `/fastshot`, `/vision`,
    /// or `/oneshot`. Route selection is made by the unified dispatcher
    /// in `stream()`; this method just sends the bytes.
    ///
    /// Wire shape (identical across all three paths — only `/vision`
    /// adds the `images: [...]` array; see protocol spec
    /// `docs/adr` notes 2026-05-15):
    ///
    /// ```text
    /// POST {base_url}/{fastshot|vision|oneshot}
    /// {
    ///   "prompt": "...",
    ///   "max_tokens": N,
    ///   "options": { "temperature": 0.7 },
    ///   "stream": true,
    ///   "model": "rsclaw-flash-v1"
    /// }
    /// ```
    ///
    /// Response is OAI chat.completion.chunk SSE. We always stream —
    /// the agent's stream consumer collapses to a single `Done` event
    /// for non-streaming callers. Reuses the OpenAI SSE chunk parser
    /// since the response shape is byte-for-byte identical.
    async fn stream_oneshot(&self, req: LlmRequest, path: &'static str) -> Result<LlmStream> {
        use futures::StreamExt;

        let prompt = flatten_prompt_for_oneshot(&req);
        if prompt.trim().is_empty() {
            anyhow::bail!("rsclaw {path}: empty prompt after flattening req.messages");
        }

        let mut body = serde_json::Map::new();
        body.insert("prompt".to_owned(), Value::String(prompt));
        body.insert("stream".to_owned(), Value::Bool(true));
        if let Some(mt) = req.max_tokens {
            body.insert("max_tokens".to_owned(), Value::from(mt));
        }
        // Hard-bind the model id to the endpoint per the fleet's
        // model-slot whitelist:
        //   /fastshot → rsclaw-flash-v1
        //   /vision   → rsclaw-vision-v1
        //   /oneshot  → rsclaw-agent-v1
        // The dispatch chain in `route_for` (rules 1–3 + 5–6) already
        // routes requests here based on the caller's model hint or
        // endpoint hint, so by the time we land in this function the
        // path uniquely determines which slot the worker accepts.
        // Forwarding `req.model` verbatim risks
        // `model_slot_mismatch` 400s when the caller mixes
        // (model=anthropic/..., endpoint=Flash) — we already routed to
        // /fastshot, but the wire model field would have been wrong.
        // Stamping the canonical id keeps callers from having to know
        // the exact slot strings.
        let canonical_model = match path {
            "/fastshot" => "rsclaw-flash-v1",
            "/vision" => "rsclaw-vision-v1",
            _ => "rsclaw-agent-v1", // /oneshot and any future stateless variant
        };
        body.insert(
            "model".to_owned(),
            Value::String(canonical_model.to_owned()),
        );
        let mut options = serde_json::Map::new();
        if let Some(t) = req.temperature {
            options.insert("temperature".to_owned(), super::json_f32(t));
        }
        if !options.is_empty() {
            body.insert("options".to_owned(), Value::Object(options));
        }
        if path == "/vision" {
            let images = extract_images_for_oneshot(&req);
            if images.is_empty() {
                anyhow::bail!("rsclaw /vision: request has no image content");
            }
            body.insert(
                "images".to_owned(),
                Value::Array(images.into_iter().map(Value::String).collect()),
            );
        }
        let body = Value::Object(body);

        // Send via the same redirect-cache + 308-aware pipeline that
        // session traffic uses, so a fastshot worker pinned under the
        // LB benefits from the same per-origin caching as the primary
        // pool.
        let resp = self
            .send_following_redirects(path, &body, Some(Duration::from_secs(60)))
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("rsclaw {path} failed {status}: {body}");
        }

        // Native rsclaw fastshot/vision SSE — distinct from the
        // primary sessions endpoint's OAI-style frames. The three
        // event types per `docs/fastshot-vision-protocol.md §3`:
        //
        //   data: {"type":"delta","content":"..."}
        //   data: {"type":"done","finish_reason":"stop","usage":{...}}
        //   data: {"type":"error","error":"..."}
        //   data: [DONE]
        //
        // Line buffering + UTF-8 boundary handling mirrors the
        // openai.rs implementation (worker can split frames across
        // TCP segments) but the JSON shape is fastshot-native so we
        // parse it locally.
        let byte_stream = resp.bytes_stream();
        let line_buffer = Arc::new(tokio::sync::Mutex::new(String::new()));
        let utf8_remainder = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let event_stream = byte_stream
            .map_err(|e| anyhow::anyhow!("stream read error: {e}"))
            .then(move |chunk| {
                let line_buffer = line_buffer.clone();
                let utf8_remainder = utf8_remainder.clone();
                async move {
                    parse_oneshot_sse_chunk(chunk, &line_buffer, &utf8_remainder).await
                }
            })
            .flat_map(|events| futures::stream::iter(events));
        Ok(Box::pin(event_stream) as LlmStream)
    }

    async fn turn(
        &self,
        session_id: &str,
        delta: &TurnDelta,
        req: &LlmRequest,
    ) -> Result<TurnOutcome> {
        let path = format!("/sessions/{}/turn", session_id);
        let body = TurnReq {
            delta,
            options: Some(TurnOptions::from_request(req)),
            stream: true,
        };
        // No per-hop builder timeout: reqwest's `.timeout()` is a
        // total deadline that includes the streaming body, so a 180s
        // cap would kill long generations (reasoning models with
        // extended thinking + large outputs routinely run past three
        // minutes). Instead bound only the time-to-response-headers
        // (PLUS any redirect-following hops along the way) with
        // `tokio::time::timeout` around the entire helper call so a
        // wedged server still surfaces fast — once headers arrive,
        // the body stream is allowed to take as long as it needs.
        // Connection liveness during streaming is covered by the
        // client-level `tcp_keepalive(30s)` configured above.
        let send_fut = self.send_following_redirects(&path, &body, None);
        let resp = match tokio::time::timeout(TURN_HEADERS_TIMEOUT, send_fut).await {
            Ok(r) => r?,
            Err(_) => anyhow::bail!(
                "rsclaw turn: timed out waiting for response headers after {}s ({}{})",
                TURN_HEADERS_TIMEOUT.as_secs(),
                self.base_url,
                path,
            ),
        };
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // 404 session_not_found (slot evicted), 409 version_drift
            // (pinned node upgraded past our rsclaw_version) and 503
            // backend_unavailable (pinned node gone via heartbeat
            // timeout) all share the same recovery path: replay against
            // current rsclaw_version and retry. Other 404s — typically
            // a misrouted request hitting a CDN/proxy 404 page — should
            // bail with the upstream body so operators can see the real
            // error instead of looping forever in replay.
            if is_session_evicted(status, &body) {
                return Ok(TurnOutcome::SessionNotFound);
            }
            anyhow::bail!("rsclaw turn failed {status}: {body}");
        }

        let byte_stream = resp.bytes_stream();
        let line_buffer = Arc::new(tokio::sync::Mutex::new(String::new()));
        let utf8_remainder = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let event_stream = byte_stream
            .map_err(|e| anyhow::anyhow!("stream read error: {e}"))
            .then(move |chunk| {
                let line_buffer = line_buffer.clone();
                let utf8_remainder = utf8_remainder.clone();
                async move {
                    parse_sse_chunk(chunk, &line_buffer, &utf8_remainder).await
                }
            })
            .flat_map(futures::stream::iter);

        Ok(TurnOutcome::Stream(Box::pin(event_stream)))
    }
}

enum TurnOutcome {
    Stream(LlmStream),
    SessionNotFound,
}

// ---------------------------------------------------------------------------
// Wire types — mirror rsclaw-protocol.md §2
// ---------------------------------------------------------------------------

/// Wire shape of `dynamic_prefix` per protocol §2.1.2 (post-2026-05-16
/// rename: the per-session non-hashed segment is named `user_system`,
/// not `user_suffix`). Both `system` and `tools` participate in the
/// worker's content-addressed LRU hash; `user_system` does NOT — it's
/// stored as a per-session prefix layer between `base` (builtin tools
/// + base_system) and `session_tail` (history + delta).
#[derive(Debug, Serialize)]
struct DynamicPrefixWire<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    system: &'a str,
    /// All tools — builtin first, then per-client. rsclaw-server's
    /// post-rename contract does NOT carry a separate top-level
    /// `user_tools` field (its `backend/rsclaw_llm.rs` tests assert
    /// `body.get("user_tools").is_none()`), so the split is encoded
    /// purely as an ORDERING within this array. Builtins-first means
    /// the chat-template-rendered byte prefix is stable across every
    /// client of this RsClaw version, giving the worker's suffix-stable
    /// LCP trim something to share between clients whose only
    /// difference is per-machine MCP / plugin tools.
    tools: &'a [Value],
    /// Per-session text that does NOT participate in the worker's
    /// system+tools hash. Maps to the worker's `user_system` KV cache
    /// layer. Server treats this as opaque text to prefill after the
    /// base layer and before session_tail.
    #[serde(skip_serializing_if = "str::is_empty")]
    user_system: &'a str,
}

#[derive(Debug, Serialize)]
struct CreateSessionReq<'a> {
    /// Protocol §2.1.1 — namespaced `<ns>/<ver>`. rsclaw-server still
    /// accepts the legacy `rsclaw_version` field name as an alias, but
    /// we always send the post-rename name on new traffic.
    prefix_id: &'a str,
    /// Bare model id (no `rsclaw/` namespace prefix) — required since
    /// 2026-05 to route the request to the correct model slot on the
    /// worker. The session retains this binding for its lifetime, so
    /// `/turn` and `/replay` traffic against the same session_id never
    /// needs to repeat it. Strip the `rsclaw/` namespace before
    /// sending because the server records the bare id.
    model: &'a str,
    /// Hybrid mode (§2.1.3) — always sent. When `prefix_id` hits the
    /// static registry the worker forks from there; otherwise the
    /// dynamic-LRU keyed by hash of `system + tools` is used.
    dynamic_prefix: DynamicPrefixWire<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<TurnOptions>,
}

#[derive(Debug, Serialize)]
struct ReplayReq<'a> {
    prefix_id: &'a str,
    /// Same bare model id contract as `CreateSessionReq` — replay
    /// rebuilds the session from scratch, so the model binding must be
    /// declared again. Worker returns
    /// `missing_model` 400 if omitted.
    model: &'a str,
    dynamic_prefix: DynamicPrefixWire<'a>,
    history: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<TurnOptions>,
}

/// Wire body for `POST /v1/agent/sessions/<id>/compact` per protocol §2.4.
/// In-place splice: keep first `keep_head_messages` messages' KV unchanged,
/// drop the middle KV pages, prefill `summary` in place, keep the last
/// `keep_tail_messages` messages' KV unchanged. Server returns the same
/// `session_id` (no slot reallocation).
///
/// `expected_msgs_count` is optimistic concurrency — gateway tells server
/// what total `msgs_count` it thinks the session has right now. Mismatch
/// returns 409 and gateway must fall back to `/sessions/replay`.
///
/// The wire excludes `prefix_id` / `dynamic_prefix` because compact targets
/// an already-open session by `session_id`; the slot's existing prefix
/// stays bound to whatever was used at open time.
#[derive(Debug, Serialize)]
struct CompactSpliceReq<'a> {
    keep_head_messages: usize,
    summary: &'a str,
    keep_tail_messages: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_msgs_count: Option<usize>,
}

/// Response from `POST /sessions/<id>/compact`. `session_id` mirrors the
/// path parameter and exists for sanity-check / log correlation only —
/// the §2.4 spec guarantees it does NOT change across splice. `msgs_count`
/// and `tokens_count` are server's authoritative post-splice counts; the
/// gateway uses them for cross-check against its own local computation
/// (head_count + 1 summary + tail_count, summed tokens of those).
#[derive(Debug, Deserialize, Clone)]
struct CompactSpliceResp {
    #[allow(dead_code)] // kept for log correlation when paired with session_id-keyed metrics
    session_id: String,
    msgs_count: usize,
    tokens_count: usize,
}

#[derive(Debug, Deserialize, Clone)]
struct CreateSessionResp {
    session_id: String,
    /// Post-rename canonical id from protocol §2.1.6 (`<namespace>/<id>`).
    ///
    /// Modeled as `Option<String>` rather than `#[serde(default)]
    /// String` so we accept three shapes:
    ///   - field absent              → `None` (replay path)
    ///   - field present, string     → `Some(v)` (open path)
    ///   - field present, JSON null  → `None`
    ///
    /// The null case is the trap: serde's `String` deserializer rejects
    /// `null` outright with `invalid type: null, expected a string` and
    /// the whole response parse dies. Upstream nodes occasionally emit
    /// `"prefix_id": null` mid-roll — accept that gracefully.
    ///
    /// We do NOT use `#[serde(alias = "rsclaw_version")]` here. The
    /// pre-rename `rsclaw_version` field is being dropped server-side;
    /// in the meantime some builds still emit it alongside `prefix_id`
    /// in the same payload. With an `alias` serde would treat the
    /// second occurrence as a duplicate field and bail the whole
    /// response parse with `duplicate field`prefix_id``, which surfaced
    /// to callers as the opaque `rsclaw open: parse response` error
    /// (seen in production e2e against `:8443`). Without the alias,
    /// the legacy `rsclaw_version` field is just an unknown key serde
    /// ignores by default — exactly what we want as the field gets
    /// retired.
    ///
    /// Parsed for forward compat / observability only. NOT used as the
    /// session cache key — see the SessionEntry construction sites for
    /// why caching the upstream canonical breaks alias-based requests.
    #[serde(default)]
    #[allow(dead_code)]
    prefix_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct TurnReq<'a> {
    #[serde(flatten)]
    delta: &'a TurnDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<TurnOptions>,
    stream: bool,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum TurnDelta {
    User { user_message: String },
    Tools { tool_results: Vec<ToolResultDelta> },
}

impl TurnDelta {
    fn from_request(req: &LlmRequest) -> Result<Self> {
        let last = req
            .messages
            .last()
            .context("rsclaw: empty messages, no delta to send")?;
        if !matches!(last.role, Role::User | Role::Tool) {
            anyhow::bail!(
                "rsclaw: last message must be User or Tool, got {:?}",
                last.role
            );
        }

        // When the assistant calls N tools in parallel, the runtime
        // queues N consecutive Role::Tool messages (one per result).
        // Protocol §2.3 requires a single turn() carry ALL of them in
        // one tool_results array, else server bails with 400
        // tool_results_incomplete. Walk back from the tail collecting
        // every consecutive Tool message; stop at the first non-Tool.
        if matches!(last.role, Role::Tool) {
            let mut tail: Vec<&Message> = Vec::new();
            for m in req.messages.iter().rev() {
                if matches!(m.role, Role::Tool) {
                    tail.push(m);
                } else {
                    break;
                }
            }
            tail.reverse();
            let mut tool_results: Vec<ToolResultDelta> = Vec::new();
            for m in tail {
                if let MessageContent::Parts(parts) = &m.content {
                    for p in parts {
                        if let ContentPart::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = p
                        {
                            tool_results.push(ToolResultDelta {
                                tool_use_id: tool_use_id.clone(),
                                content: content.clone(),
                                is_error: is_error.unwrap_or(false),
                            });
                        }
                    }
                }
            }
            if tool_results.is_empty() {
                anyhow::bail!("rsclaw: trailing Tool message(s) carried no tool_result parts");
            }
            return Ok(TurnDelta::Tools { tool_results });
        }

        // Role::User branch — the trailing message is treated as one
        // user_message. Empty content (Text("") or Parts with only
        // empty Text fragments) bails: protocol §2.3 requires a real
        // delta, and silently shipping "" still bills a prefill on the
        // upstream slot. Mirrors the empty-tool_results bail above.
        let mut user_text = String::new();
        match &last.content {
            MessageContent::Text(t) => user_text.push_str(t),
            MessageContent::Parts(parts) => {
                for p in parts {
                    if let ContentPart::Text { text } = p {
                        user_text.push_str(text);
                    }
                }
            }
        }
        if user_text.is_empty() {
            anyhow::bail!("rsclaw: last message has no usable content for delta")
        }
        Ok(TurnDelta::User { user_message: user_text })
    }
}

#[derive(Debug, Serialize)]
struct ToolResultDelta {
    tool_use_id: String,
    content: String,
    #[serde(default)]
    is_error: bool,
}

/// Serializer for `Option<f32>` that routes the value through
/// `super::json_f32` (rounds to 2 decimal places) instead of the default
/// f32 → f64 path which leaks IEEE 754 precision artefacts (0.6_f32 →
/// 0.6000000238418579 in JSON output). Mirrors what every other provider
/// in this crate does manually with `body["temperature"] = json_f32(t)`.
fn ser_opt_f32<S: serde::Serializer>(
    v: &Option<f32>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match v {
        None => s.serialize_none(),
        Some(f) => super::json_f32(*f).serialize(s),
    }
}

#[derive(Debug, Serialize, Clone, Default)]
struct TurnOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f32")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f32")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    idle_ttl_secs: Option<u32>,
}

impl TurnOptions {
    fn from_request(req: &LlmRequest) -> Self {
        Self {
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: None,
            enable_thinking: req.thinking_budget.map(|b| b > 0),
            stop: None,
            idle_ttl_secs: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Request splitting (LlmRequest → protocol fields)
// ---------------------------------------------------------------------------

/// Maps an `LlmRequest` onto the protocol's split fields per
/// rsclaw-protocol §2.1 (post-rename, hybrid path).
///
/// When the runtime populated `req.system_shared` / `req.user_system`
/// (kvCacheMode=2 path on the main agent loop), the split lands in the
/// "real" hybrid shape:
/// - `dynamic_system`        ← shared system prefix (byte-stable across
///   every RsClaw client of this version) → wire `dynamic_prefix.system`
/// - `dynamic_user_system`   ← per-machine non-hashed segment → wire
///   `dynamic_prefix.user_system` (worker layer-2 cache key
///   intentionally EXCLUDES this, so it can vary per session without
///   collapsing the hit rate)
/// - `dynamic_tools`         ← all tools, builtin-first then per-client.
///   Encoded as a single array because rsclaw-server's post-rename
///   contract drops top-level `user_tools` (verified by its own
///   `body.get("user_tools").is_none()` test). The ordering preserves a
///   byte-stable rendered prefix across clients of the same RsClaw
///   version regardless of their per-machine MCP/plugin tools.
///
/// When the split fields are missing (internal sessions / non-runtime
/// callers) we degrade gracefully: stuff `req.system` into
/// `dynamic_system`, every tool into `dynamic_tools` in input order,
/// leave `dynamic_user_system` empty. Same effective cache behaviour
/// as before this change.
struct SplitRequest<'a> {
    /// Namespaced `rsclaw/<id>` per protocol §2.10.1.
    prefix_id: String,
    /// Bare model id with the `rsclaw/` namespace prefix stripped. Required
    /// in the wire body for both `POST /sessions` and `/sessions/replay`
    /// as of 2026-05; sessions carry this binding for their lifetime.
    model: String,
    dynamic_system: &'a str,
    dynamic_user_system: &'a str,
    dynamic_tools: Vec<Value>,
    options: TurnOptions,
}

/// Dump the full request shape for one turn so the operator can
/// replay the same logical input against several rsclaw-llm endpoints
/// (rsclaw stateful `/sessions/<id>/turn`, `/sessions/replay`, vanilla
/// `/v1/chat/completions`) and compare the model's output. Used to
/// bisect a "truncation happens via rsclaw protocol but not via OpenAI
/// compat" symptom into "rsclaw-llm side problem" vs "model side
/// problem".
///
/// Writes one JSON file per turn to:
///   `<base_dir>/debug/turn-<unix_ms>-<session_suffix>.json`
///
/// Gated on `RSCLAW_DUMP_TURN` env var being set (any non-empty value).
/// Write failures are logged at WARN but don't abort the turn.
fn dump_turn_for_debug(
    session_key: &str,
    entry: &SessionEntry,
    split: &SplitRequest<'_>,
    delta: &TurnDelta,
    req: &LlmRequest,
) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Compose the replay history slice (everything before the trailing
    // delta — same logic `replay()` uses on the recovery path).
    let history_owned: Vec<&Message> = history_for_replay(&req.messages).iter().collect();
    let history_values = serialize_replay_history(&history_owned);

    // Equivalent OpenAI-compatible chat body — message-array + tools
    // shape that vanilla `/v1/chat/completions` accepts. The model id
    // is the bare form (no `rsclaw/` namespace prefix) since the local
    // llama-server's OpenAI surface doesn't recognize prefixed names.
    let openai_model = req.model.strip_prefix("rsclaw/").unwrap_or(&req.model);
    let openai_messages: Vec<Value> = req
        .messages
        .iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .collect();
    let openai_tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect();

    let opts = TurnOptions::from_request(req);

    // Wire body of the actual `/sessions/<id>/turn` request we're about
    // to send. Must match what `Provider::turn()` puts on the wire: the
    // TurnDelta uses `#[serde(flatten)]` inside TurnReq, so `user_message`
    // or `tool_results` sit at the TOP level alongside `options`/`stream`.
    // Nesting them under a `delta` wrapper would make the dump non-replayable
    // — the worker returns 400 `invalid_request: turn must include exactly
    // one of user_message (string) or tool_results (non-empty array)`.
    // Canonicalize through to_canonical_value so the dumped bytes match
    // the actual wire bytes the provider sends. The crate uses
    // serde_json's `preserve_order` feature, which keeps insertion-order
    // keys; the wire send path runs the body through `to_canonical_value`
    // (BTreeMap-sorted) so worker-side hashes are byte-stable. Without
    // the same pass on the dump, an operator using RSCLAW_DUMP_TURN to
    // bisect a "truncation in rsclaw protocol but not in OpenAI compat"
    // symptom would see *different* bytes from what `/sessions/.../turn`
    // received — masking exactly the kind of byte-level bug the dump
    // tool exists to diagnose. R1 review I2.
    let turn_body = to_canonical_value(
        serde_json::to_value(&TurnReq {
            delta,
            options: Some(opts.clone()),
            stream: true,
        })
        .unwrap_or(Value::Null),
    );

    // Wire body that would rehydrate this session from scratch via
    // `/sessions/replay`. Useful when the session_id is no longer alive
    // on the worker and the operator wants to recreate the exact state.
    let replay_body = to_canonical_value(json!({
        "prefix_id": split.prefix_id,
        "dynamic_prefix": {
            "system": split.dynamic_system,
            "tools": split.dynamic_tools,
            "user_system": split.dynamic_user_system,
        },
        "history": history_values,
        "options": serde_json::to_value(&opts).unwrap_or(Value::Null),
    }));

    let dump = json!({
        "schema_version": 1,
        "timestamp_ms": now_ms,
        "session_key": session_key,
        "model": req.model,
        "rsclaw_session": {
            "session_id": entry.session_id,
            "prefix_id": entry.prefix_id,
            "last_seen_msgs_len": entry.last_seen_msgs_len,
        },
        "llm_request_summary": {
            "msg_count": req.messages.len(),
            "tool_count": req.tools.len(),
            "system_len": req.system.as_deref().map(|s| s.len()).unwrap_or(0),
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "kv_cache_mode": req.kv_cache_mode,
        },
        "rsclaw_turn_body": turn_body,
        "rsclaw_replay_body": replay_body,
        "openai_chat_completions_body": {
            "model": openai_model,
            "messages": openai_messages,
            "tools": openai_tools,
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
            "stream": true,
        },
        "replay_instructions": [
            "Pick ONE of the three replay paths and POST against the worker:",
            "  A. Stateful turn against a LIVE session (only works while session is alive):",
            "     curl -X POST $BASE/sessions/<session_id>/turn -d @<this-file>[.rsclaw_turn_body]",
            "  B. Re-hydrate then turn — recreates session deterministically:",
            "     curl -X POST $BASE/sessions/replay  -d @<this-file>[.rsclaw_replay_body]",
            "     curl -X POST $BASE/sessions/<new_session_id>/turn -d @<this-file>[.rsclaw_turn_body]",
            "  C. Stateless OpenAI-compat for comparison (no session, full history each time):",
            "     curl -X POST $BASE/v1/chat/completions -d @<this-file>[.openai_chat_completions_body]"
        ]
    });

    // Pick a short suffix for the file name — session_id's tail hex is
    // unique enough to disambiguate within one millisecond.
    let sess_suffix: String = entry
        .session_id
        .rsplit('_')
        .next()
        .unwrap_or("unknown")
        .chars()
        .take(8)
        .collect();
    let dir = crate::config::loader::base_dir().join("debug");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, "RSCLAW_DUMP_TURN: create_dir_all failed");
        return;
    }
    let path = dir.join(format!("turn-{now_ms}-{sess_suffix}.json"));
    match serde_json::to_string_pretty(&dump) {
        Ok(s) => match std::fs::write(&path, s) {
            Ok(_) => tracing::info!(path = %path.display(), "RSCLAW_DUMP_TURN: turn dumped"),
            Err(e) => tracing::warn!(error = %e, path = %path.display(), "RSCLAW_DUMP_TURN: write failed"),
        },
        Err(e) => tracing::warn!(error = %e, "RSCLAW_DUMP_TURN: serialize failed"),
    }
}

/// Walk a `serde_json::Value`, returning a deep copy with every
/// `Object` map's keys reordered alphabetically. Arrays preserve
/// input order (they're inherently positional). Primitives are
/// cloned as-is.
///
/// Purpose: the crate-wide `preserve_order` feature on `serde_json`
/// makes `Map<String, Value>` an `IndexMap` that keeps insertion
/// order — fine for round-tripping JSON5 configs, but it lets the
/// non-determinism of upstream `HashMap` iteration leak into the
/// wire JSON sent to rsclaw-server. The worker hashes the
/// `dynamic_prefix.tools` payload byte-by-byte to form its prefix
/// cache key, so even one swapped key triggers a `dynamic_miss`
/// and a full prefill (~200s on a 28k-token prefix). Sorting keys
/// here gives a content-addressed canonical form: identical tool
/// definitions → identical bytes → identical hash → `dynamic_hit`.
fn to_canonical_value(v: serde_json::Value) -> serde_json::Value {
    use std::collections::BTreeMap;
    match v {
        serde_json::Value::Object(map) => {
            // BTreeMap forces alphabetical key order regardless of
            // the original IndexMap's insertion sequence.
            let sorted: BTreeMap<String, serde_json::Value> =
                map.into_iter().map(|(k, v)| (k, to_canonical_value(v))).collect();
            let canon: serde_json::Map<String, serde_json::Value> = sorted.into_iter().collect();
            serde_json::Value::Object(canon)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(to_canonical_value).collect())
        }
        other => other,
    }
}

fn split_request<'a>(req: &'a LlmRequest, prefix_id: &str) -> Result<SplitRequest<'a>> {
    // `prefix_id` is config-driven (provider-level, default
    // [`RSCLAW_DEFAULT_PREFIX_ID`]) — NOT derived from `req.model` or
    // any per-turn data. Protocol §2.10.1 only mandates exactly one
    // `/` separator; the provider builder is responsible for keeping
    // the wire shape valid, so this function passes it through as-is.
    let prefix_id = prefix_id.to_owned();

    // `model` is required in the open/replay wire body since 2026-05.
    // Strip the `rsclaw/` namespace prefix so the server records the
    // bare slot id (e.g. `rsclaw-agent-v1`) — passing the namespaced
    // form trips the worker's model-slot whitelist check.
    let model = req
        .model
        .strip_prefix("rsclaw/")
        .unwrap_or(req.model.as_str())
        .to_owned();
    if model.is_empty() {
        anyhow::bail!("rsclaw: req.model is empty; cannot open session without a model id");
    }

    // Each tool's wire JSON is `to_canonical_value`-flattened to give
    // byte-stable output across gateway runs. Without this pass the
    // serialized `input_schema` carries whatever key order `serde_json`
    // observed when each field was inserted — and because the crate is
    // compiled with the `preserve_order` feature globally, ordering is
    // whatever the source `HashMap` / macro / derive happened to emit,
    // which is non-deterministic across runs. A flipped key in any of
    // the dozens of schema entries flips the worker-side dynamic_prefix
    // hash, forces `dynamic_miss`, and triggers a fresh 30-200s prefill
    // on every gateway restart even though the agent's tool list is
    // logically unchanged. Worker hash is content-addressed, so
    // alphabetical key order alone makes "same tools" map to the same
    // slot reliably.
    let tool_json = |t: &super::ToolDef| {
        to_canonical_value(json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.parameters,
        }))
    };

    let (dynamic_system, dynamic_user_system, dynamic_tools) =
        if req.system_shared.is_some() || req.user_system.is_some() {
            // Real split — sort tools [builtin..., per-client...] so
            // the rendered chat-template prefix is byte-stable across
            // every client of this version up to the per-client tool
            // boundary.
            let mut builtin_t = Vec::new();
            let mut user_t = Vec::new();
            for t in &req.tools {
                if crate::agent::prompt_builder::BUILTIN_TOOL_NAMES
                    .contains(&t.name.as_str())
                {
                    builtin_t.push(tool_json(t));
                } else {
                    user_t.push(tool_json(t));
                }
            }
            builtin_t.extend(user_t);
            (
                req.system_shared.as_deref().unwrap_or(""),
                req.user_system.as_deref().unwrap_or(""),
                builtin_t,
            )
        } else {
            // No split available — collapse everything into the
            // dynamic prefix in input order. Per-client LRU key is over
            // the full system + tools, so every distinct caller still
            // gets its own slot (same as pre-split behaviour).
            let all_tools: Vec<Value> = req.tools.iter().map(tool_json).collect();
            (
                req.system.as_deref().unwrap_or(""),
                "",
                all_tools,
            )
        };

    Ok(SplitRequest {
        prefix_id,
        model,
        dynamic_system,
        dynamic_user_system,
        dynamic_tools,
        options: TurnOptions::from_request(req),
    })
}

/// Returns the history slice to send to `/sessions/replay`: every
/// message except the trailing delta (which `turn()` will re-send).
/// Empty input returns an empty slice — replay can still hydrate a
/// fresh session with no prior turns.
///
/// When the assistant calls N tools in parallel the runtime queues N
/// consecutive `Role::Tool` messages, and `TurnDelta::from_request`
/// folds ALL of them into a single tool_results delta (protocol §2.3
/// requires it). To keep the two sides symmetric the history slice
/// must drop every consecutive trailing `Role::Tool` — dropping just
/// one would leave the other N-1 in history, the server would replay
/// them into the KV, and then the turn would re-send the same
/// Flatten an `LlmRequest`'s system + messages into a single prompt
/// string for the one-shot `/fastshot` and `/vision` endpoints, which
/// take a bare `prompt` field instead of OpenAI-style messages.
///
/// Concatenation order: system → message texts in order, joined by
/// blank lines. Image parts are skipped here (the caller pulls them
/// into the `images` array via `extract_images_for_oneshot`). Tool
/// use/result parts and reasoning parts are also skipped — fastshot
/// is a tool-less endpoint and historical tool traffic isn't
/// meaningful in that context.
fn flatten_prompt_for_oneshot(req: &LlmRequest) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(sys) = req.system.as_deref() {
        let trimmed = sys.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_owned());
        }
    }
    for msg in &req.messages {
        match &msg.content {
            MessageContent::Text(t) => {
                let trimmed = t.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_owned());
                }
            }
            MessageContent::Parts(content_parts) => {
                for p in content_parts {
                    if let ContentPart::Text { text } = p {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            parts.push(trimmed.to_owned());
                        }
                    }
                }
            }
        }
    }
    parts.join("\n\n")
}

/// Parse one SSE chunk from the native fastshot/vision wire and
/// emit `StreamEvent`s. Buffers partial lines across chunks so a
/// frame split mid-JSON resolves cleanly; mirrors the strategy used
/// by `openai::parse_sse_chunk_with_buffer` but with the
/// fastshot-native JSON shape.
async fn parse_oneshot_sse_chunk(
    chunk: anyhow::Result<bytes::Bytes>,
    line_buffer: &tokio::sync::Mutex<String>,
    utf8_remainder: &tokio::sync::Mutex<Vec<u8>>,
) -> Vec<anyhow::Result<StreamEvent>> {
    let bytes = match chunk {
        Ok(b) => b,
        Err(e) => return vec![Err(e)],
    };

    // Carry forward any UTF-8 continuation bytes that landed at the
    // tail of the previous chunk — without this, CJK / emoji
    // characters that straddle a chunk boundary corrupt into U+FFFD.
    let mut remainder = utf8_remainder.lock().await;
    let combined = if remainder.is_empty() {
        bytes.to_vec()
    } else {
        let mut c = std::mem::take(&mut *remainder);
        c.extend_from_slice(&bytes);
        c
    };
    let text: String = match std::str::from_utf8(&combined) {
        Ok(t) => {
            drop(remainder);
            t.to_owned()
        }
        Err(e) => {
            let valid_up_to = e.valid_up_to();
            *remainder = combined[valid_up_to..].to_vec();
            drop(remainder);
            if valid_up_to == 0 {
                return Vec::new();
            }
            // SAFETY: valid_up_to is at a valid UTF-8 boundary by
            // construction of the `Utf8Error`.
            unsafe { std::str::from_utf8_unchecked(&combined[..valid_up_to]) }.to_owned()
        }
    };

    let mut buffer = line_buffer.lock().await;
    buffer.push_str(&text);
    let Some(last_newline) = buffer.rfind('\n') else {
        return Vec::new();
    };
    let complete = buffer[..last_newline].to_owned();
    let leftover = buffer[last_newline + 1..].to_owned();
    buffer.clear();
    buffer.push_str(&leftover);
    drop(buffer);

    let mut events: Vec<anyhow::Result<StreamEvent>> = Vec::new();
    for line in complete.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim_start();
        if payload.is_empty() {
            continue;
        }
        if payload == "[DONE]" {
            // Spec §3: server always sends `data: [DONE]` after the
            // terminal frame. We only emit our own Done if the
            // worker never sent one — typical path is `done` event
            // first (which we already turned into StreamEvent::Done
            // with usage) then `[DONE]` which we swallow here.
            continue;
        }
        let val: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => {
                tracing::debug!(payload, "rsclaw fastshot: ignoring unparseable SSE line");
                continue;
            }
        };
        let ty = val.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "delta" => {
                if let Some(content) = val.get("content").and_then(Value::as_str) {
                    if !content.is_empty() {
                        events.push(Ok(StreamEvent::TextDelta(content.to_owned())));
                    }
                }
            }
            "done" => {
                let usage = val
                    .get("usage")
                    .and_then(Value::as_object)
                    .map(|u| TokenUsage {
                        input: extract_usage_count(u, &["input_tokens", "prompt_tokens", "input"]),
                        output: extract_usage_count(
                            u,
                            &["output_tokens", "completion_tokens", "output"],
                        ),
                    });
                events.push(Ok(StreamEvent::Done { usage }));
            }
            "error" => {
                // Per §4.2 the error payload is `{code, message}`.
                let err = val.get("error");
                let code = err
                    .and_then(|e| e.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let detail = err
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let msg = match (code.is_empty(), detail.is_empty()) {
                    (false, false) => format!("rsclaw stream error [{code}]: {detail}"),
                    (false, true) => format!("rsclaw stream error [{code}]"),
                    (true, false) => format!("rsclaw stream error: {detail}"),
                    (true, true) => "rsclaw stream error".to_string(),
                };
                events.push(Ok(StreamEvent::Error(msg)));
            }
            "thinking" => {
                if let Some(s) = val.get("content").and_then(Value::as_str)
                    && !s.is_empty()
                {
                    events.push(Ok(StreamEvent::ReasoningDelta(s.to_string())));
                }
            }
            "tool_call" => {
                let id = val
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = val
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input = val
                    .get("input")
                    .cloned()
                    .filter(Value::is_object)
                    .unwrap_or(Value::Object(Default::default()));
                events.push(Ok(StreamEvent::ToolCall { id, name, input }));
            }
            other => {
                tracing::debug!(ty = other, payload, "rsclaw fastshot: unknown event type");
            }
        }
    }
    events
}

/// Pull every image URL/data-URI out of an `LlmRequest`'s message
/// content parts, preserving order. Used by the `/vision` one-shot
/// endpoint which expects an `images: [...]` array alongside the
/// flattened prompt.
fn extract_images_for_oneshot(req: &LlmRequest) -> Vec<String> {
    let mut images = Vec::new();
    for msg in &req.messages {
        if let MessageContent::Parts(parts) = &msg.content {
            for p in parts {
                if let ContentPart::Image { url } = p {
                    if !url.is_empty() {
                        images.push(url.clone());
                    }
                }
            }
        }
    }
    images
}

/// tool_results, hydrating duplicates.
///
/// For a User-trailing list (the iter-1 case after
/// `normalize_trailing_system` ran) we drop exactly one message.
fn history_for_replay(messages: &[Message]) -> &[Message] {
    if messages.is_empty() {
        return messages;
    }
    let last = &messages[messages.len() - 1];
    if matches!(last.role, Role::Tool) {
        let mut keep = messages.len();
        while keep > 0 && matches!(messages[keep - 1].role, Role::Tool) {
            keep -= 1;
        }
        &messages[..keep]
    } else {
        &messages[..messages.len() - 1]
    }
}

/// Partition a history slice into (non-system messages, concatenated
/// system text). The runtime threads `Role::System` messages through
/// the conversation list for plugins/skills/ctx blocks, but protocol
/// §2.2 only accepts `user` / `assistant` in history — so we lift the
/// system text out and let the caller append it to `user_system`.
/// Order of system blocks is preserved within the returned String;
/// blocks are joined with a blank line. Non-text content on a
/// `Role::System` message is dropped (system messages are documented
/// as text-only in the runtime).
fn split_system_messages(messages: &[Message]) -> (Vec<&Message>, String) {
    let mut filtered: Vec<&Message> = Vec::with_capacity(messages.len());
    let mut sys_parts: Vec<String> = Vec::new();
    for m in messages {
        if matches!(m.role, Role::System) {
            // Text(t) and Parts(...) must skip empties symmetrically —
            // otherwise an empty System(Text("")) leaks a blank entry
            // into sys_parts and pollutes user_system with a leading
            // `\n\n` once `sys_parts.join("\n\n")` runs.
            let txt = match &m.content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Parts(parts) => {
                    let mut joined = String::new();
                    for p in parts {
                        if let ContentPart::Text { text } = p {
                            joined.push_str(text);
                        }
                    }
                    joined
                }
            };
            if !txt.is_empty() {
                sys_parts.push(txt);
            }
        } else {
            filtered.push(m);
        }
    }
    (filtered, sys_parts.join("\n\n"))
}

/// Pull any trailing `Role::System` messages off the end of `messages`
/// and fold their text into the preceding `Role::User` message.
///
/// The runtime appends `Role::System` blocks (dynamic /ctx, just-
/// installed skills) AFTER the User delta on the first iteration of
/// each turn (`turn_scratchpad` empty — see agent/runtime.rs). On
/// later iterations the scratchpad's Assistant/Tool entries follow,
/// so trailing-System only happens iter-1. `TurnDelta::from_request`
/// rejects a trailing System ("last message must be User or Tool"),
/// failing the whole turn — fold the text inline so the model still
/// sees the dynamic context, just as part of the user_message body.
///
/// If the message immediately before the trailing System block(s) is
/// `Role::Tool` (parallel tool_results case, theoretical — runtime
/// doesn't currently inject System after Tool, but defend anyway),
/// drop the System text. Persistent system content already lives in
/// `user_system` from the prior open/replay; only the per-iteration
/// dynamic context would be lost, which the protocol has no slot
/// for in a tool_results delta.
fn normalize_trailing_system(messages: &mut Vec<Message>) {
    let mut trailing: Vec<String> = Vec::new();
    while matches!(messages.last(), Some(m) if matches!(m.role, Role::System)) {
        let m = messages.pop().expect("matched Some above");
        let txt = match m.content {
            MessageContent::Text(t) => t,
            MessageContent::Parts(parts) => parts
                .into_iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        };
        if !txt.is_empty() {
            trailing.push(txt);
        }
    }
    if trailing.is_empty() {
        return;
    }
    trailing.reverse();
    let combined = trailing.join("\n\n");
    match messages.last_mut() {
        Some(last) if matches!(last.role, Role::User) => match &mut last.content {
            MessageContent::Text(t) => {
                if t.is_empty() {
                    *t = combined;
                } else {
                    t.push_str("\n\n");
                    t.push_str(&combined);
                }
            }
            MessageContent::Parts(parts) => {
                parts.push(ContentPart::Text { text: combined });
            }
        },
        _ => {
            // Tool / empty — nothing to fold into. Drop silently.
        }
    }
}

fn serialize_history_message(msg: &Message) -> Value {
    let role = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user",
    };
    let content = match &msg.content {
        MessageContent::Text(t) => json!(t),
        MessageContent::Parts(parts) => {
            let mapped: Vec<Value> = parts.iter().map(serialize_history_part).collect();
            json!(mapped)
        }
    };
    json!({ "role": role, "content": content })
}

fn serialize_history_part(p: &ContentPart) -> Value {
    match p {
        ContentPart::Text { text } => json!({"type":"text","text":text}),
        ContentPart::Image { url } => {
            json!({"type":"image","source":{"type":"url","url":url}})
        }
        ContentPart::ToolUse { id, name, input } => {
            json!({"type":"tool_use","id":id,"name":name,"input":input})
        }
        ContentPart::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let mut obj = json!({
                "type":"tool_result",
                "tool_use_id":tool_use_id,
                "content":content,
            });
            if let Some(e) = is_error {
                obj["is_error"] = json!(e);
            }
            obj
        }
        ContentPart::Reasoning { text } => json!({"type":"thinking","text":text}),
    }
}

/// Serialize replay history with consecutive `Role::Tool` messages
/// coalesced into one `user`-role entry whose `content` array carries
/// every `tool_result` part.
///
/// Why: when the assistant calls N tools in parallel, the runtime queues
/// N consecutive `Role::Tool` messages (one per result). `from_request`
/// already merges them into a single `tool_results` array on a live
/// turn; replay history needs the same shape per protocol §2.2 — the
/// example there shows tool_results inside ONE user-role entry, and
/// shipping N separate entries would either tokenize wrong or trip
/// `400 invalid_history` on stricter chat templates.
fn serialize_replay_history(messages: &[&Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut i = 0;
    while i < messages.len() {
        let m = messages[i];
        if !matches!(m.role, Role::Tool) {
            out.push(serialize_history_message(m));
            i += 1;
            continue;
        }
        let mut combined: Vec<Value> = Vec::new();
        while i < messages.len() && matches!(messages[i].role, Role::Tool) {
            match &messages[i].content {
                MessageContent::Parts(parts) => {
                    for p in parts {
                        if matches!(p, ContentPart::ToolResult { .. }) {
                            combined.push(serialize_history_part(p));
                        }
                    }
                }
                MessageContent::Text(_) => {
                    // Defensive: today's runtime always emits
                    // `Role::Tool` with `Parts(vec![ToolResult{..}])`,
                    // so this branch should never trigger. If it ever
                    // does — e.g. a future runtime path or a plugin
                    // injecting a synthesised tool message — the text
                    // has no `tool_use_id` to anchor it server-side
                    // (protocol §2.2 requires `tool_result` parts to
                    // pair with prior `tool_use` ids). Drop and surface
                    // a warning so we notice the contract change rather
                    // than silently producing a turn whose model
                    // response is shaped by missing context.
                    tracing::warn!(
                        "rsclaw: dropping Role::Tool with text-only content during \
                         replay (no tool_use_id to pair with — runtime contract \
                         expects Parts(ToolResult{{..}}))",
                    );
                    debug_assert!(false, "Role::Tool must carry Parts(ToolResult{{..}}); got Text");
                }
            }
            i += 1;
        }
        if !combined.is_empty() {
            out.push(json!({ "role": "user", "content": combined }));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// SSE parsing — rsclaw-native event shape (docs/client-server-integration.md §4.2)
// ---------------------------------------------------------------------------
//
// Five top-level frame types share a flat `{type, ...}` shape across every
// rsclaw-server lane that speaks this provider (`/v1/agent/sessions/*/turn`,
// `/v1/agent/fastshot`, `/v1/agent/oneshot`, `/v1/agent/vision`):
//
//   data: {"type":"delta","content":"Hello"}
//   data: {"type":"thinking","content":"reasoning fragment..."}    (reasoning models)
//   data: {"type":"tool_call","id":"...","name":"...","input":{...}} (whole frame, not accumulated)
//   data: {"type":"done","finish_reason":"...","usage":{...}}
//   data: {"type":"error","error":{"code":"...","message":"..."}}
//   data: [DONE]                                                   (SSE framing sentinel)
//
// Forward-compat rule: unknown `type` values are silently ignored — the
// server may add new types (e.g. `cache_hit_summary`) without breaking
// old clients.

async fn parse_sse_chunk(
    chunk: Result<bytes::Bytes>,
    line_buffer: &Arc<tokio::sync::Mutex<String>>,
    utf8_remainder: &Arc<tokio::sync::Mutex<Vec<u8>>>,
) -> Vec<Result<StreamEvent>> {
    let mut events: Vec<Result<StreamEvent>> = Vec::new();
    let bytes = match chunk {
        Ok(b) => b,
        Err(e) => {
            events.push(Err(e));
            return events;
        }
    };

    // Stitch this chunk onto any partial UTF-8 left over from the
    // previous chunk; decode strict; stash the trailing invalid bytes
    // (an incomplete multi-byte sequence at the chunk boundary) for
    // the next call. from_utf8_lossy would corrupt CJK / emoji deltas
    // that straddle chunk boundaries by inserting U+FFFD.
    let mut remainder = utf8_remainder.lock().await;
    let stitched: Vec<u8> = if remainder.is_empty() {
        bytes.to_vec()
    } else {
        let mut combined = std::mem::take(&mut *remainder);
        combined.extend_from_slice(&bytes);
        combined
    };
    let decoded: String = match std::str::from_utf8(&stitched) {
        Ok(s) => s.to_owned(),
        Err(e) => {
            let valid_up_to = e.valid_up_to();
            // Two distinct error shapes per `Utf8Error`:
            //   error_len() == None    → trailing bytes are an
            //     INCOMPLETE multi-byte sequence; stash them so the
            //     next chunk completes the codepoint.
            //   error_len() == Some(n) → the next n bytes are
            //     INVALID and will never become valid; advance past
            //     them. Without this advance, every subsequent chunk
            //     stitches onto the bad prefix and fails at the same
            //     position — remainder grows unboundedly and the
            //     stream stalls forever (a single stray 0xFF from a
            //     buggy proxy is enough to wedge the turn). The lost
            //     bytes are unrecoverable garbage in either reading.
            let advance_past_invalid = e.error_len().unwrap_or(0);
            *remainder = stitched[valid_up_to + advance_past_invalid..].to_vec();
            if valid_up_to == 0 {
                return events;
            }
            std::str::from_utf8(&stitched[..valid_up_to])
                .expect("valid_up_to guarantees valid UTF-8")
                .to_owned()
        }
    };
    drop(remainder);

    let mut buf = line_buffer.lock().await;
    buf.push_str(&decoded);
    while let Some(idx) = buf.find('\n') {
        let line = buf[..idx].trim_end_matches('\r').to_string();
        buf.drain(..=idx);
        // SSE has two relevant line shapes: `event: <name>` (the named
        // event channel) and `data: <json>` (the payload). Anthropic
        // shape uses both: `event:` mirrors the JSON's `"type"` field
        // for SDK compatibility. We key off the JSON `"type"` since
        // that's authoritative — `event:` lines without a body, comment
        // lines (`:keepalive`), and stray blank lines are dropped here.
        let Some(payload) = line.strip_prefix("data:").map(|s| s.trim_start_matches(' ')) else {
            continue;
        };
        // No `data: [DONE]` sentinel on the native rsclaw protocol —
        // spec §2.3.1 explicitly says message_stop is the terminator
        // and only the `/v1/chat/completions` OAI translator emits
        // [DONE]. Still defensive-skip if a misconfigured proxy injects
        // one; pushing a parse error here would tank the turn.
        if payload == "[DONE]" {
            continue;
        }
        // Skip empty `data:` payloads silently. SSE keep-alives sometimes
        // surface as `data:\n\n` (no body) when proxies translate
        // `:keepalive` comments. Pushing a parse error here would
        // surface as `Err(...)` down the stream — and runtime's
        // `match event?` (agent/runtime.rs ~4356) propagates that with
        // `?`, killing the whole turn. The empty line carries no model
        // signal; drop it the same way `[DONE]` is dropped.
        if payload.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(e) => {
                events.push(Err(anyhow::anyhow!("rsclaw SSE parse: {e}; line: {payload}")));
                continue;
            }
        };
        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            // Text fragment. Worker emits one per generated token on
            // both the session-turn lane and the fastshot lanes.
            "delta" => {
                if let Some(s) = value.get("content").and_then(Value::as_str)
                    && !s.is_empty()
                {
                    events.push(Ok(StreamEvent::TextDelta(s.to_string())));
                }
            }
            // Reasoning fragment. Only emitted by reasoning-enabled
            // models (DeepSeek-R1, o-series, Qwen3-thinking, etc.);
            // others never send it. Same `content` field as `delta`.
            "thinking" => {
                if let Some(s) = value.get("content").and_then(Value::as_str)
                    && !s.is_empty()
                {
                    events.push(Ok(StreamEvent::ReasoningDelta(s.to_string())));
                }
            }
            // Whole tool call in one frame — id, name, and input
            // arrive together. No accumulation across deltas (the
            // server flattens worker-side streaming before forwarding).
            // Per §4.2 default `input` to {} if absent or non-object so
            // downstream `.as_object()` consumers don't have to match Null.
            "tool_call" => {
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input = value
                    .get("input")
                    .cloned()
                    .filter(Value::is_object)
                    .unwrap_or(Value::Object(Default::default()));
                events.push(Ok(StreamEvent::ToolCall { id, name, input }));
            }
            // Terminal frame — carries `finish_reason` (we don't
            // propagate it; runtime treats Done as "stream complete")
            // and `usage` with worker token counts. The server
            // forwards the worker's usage object verbatim; different
            // lanes use different field names so be defensive
            // (docs/client-server-integration.md §4.3).
            "done" => {
                let usage = value
                    .get("usage")
                    .and_then(Value::as_object)
                    .map(|u| TokenUsage {
                        input: extract_usage_count(u, &["input_tokens", "prompt_tokens", "input"]),
                        output: extract_usage_count(
                            u,
                            &["output_tokens", "completion_tokens", "output"],
                        ),
                    });
                events.push(Ok(StreamEvent::Done { usage }));
            }
            // Mid-stream error frame per §4.2: `{type:"error",
            // error:{code, message}}`. Empty fields collapse to a
            // generic message rather than a confusing "[]: " prefix.
            "error" => {
                let err = value.get("error");
                let code = err
                    .and_then(|e| e.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let detail = err
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let msg = match (code.is_empty(), detail.is_empty()) {
                    (false, false) => format!("rsclaw stream error [{code}]: {detail}"),
                    (false, true) => format!("rsclaw stream error [{code}]"),
                    (true, false) => format!("rsclaw stream error: {detail}"),
                    (true, true) => "rsclaw stream error".to_string(),
                };
                events.push(Ok(StreamEvent::Error(msg)));
            }
            // Unknown types: forward-compat per §4.2 — server may add
            // new types (e.g. `cache_hit_summary`) and old clients
            // should ignore them rather than fail the turn.
            _ => {}
        }
    }
    events
}

/// Pull a usage count from a worker `usage` object, trying field
/// names in order and defaulting missing to 0. Lane-specific name
/// drift is documented in client-server-integration.md §4.3.
fn extract_usage_count(u: &serde_json::Map<String, Value>, names: &[&str]) -> u32 {
    for name in names {
        if let Some(n) = u.get(*name).and_then(Value::as_u64) {
            return n as u32;
        }
    }
    0
}

/// True when the (status, body) pair is a documented session-eviction
/// signal that the gateway should recover from via replay:
/// - `404 session_not_found` — slot evicted (LRU, idle TTL) or upstream
///   restart; per protocol §5 the recovery is `POST /sessions/replay`
/// - `409 version_drift` — pinned node upgraded past our rsclaw_version
/// - `503 backend_unavailable` — pinned node gone (heartbeat timeout)
///
/// `503 no_backend_available` (capacity exhaustion) is intentionally NOT
/// recoverable here — replay would just hit the same wall.
fn is_session_evicted(status: StatusCode, body: &str) -> bool {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (status, code.as_deref()) {
        (StatusCode::NOT_FOUND, Some("session_not_found")) => true,
        (StatusCode::CONFLICT, Some("version_drift")) => true,
        (StatusCode::SERVICE_UNAVAILABLE, Some("backend_unavailable")) => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolDef;

    #[test]
    fn origin_of_extracts_origin() {
        assert_eq!(origin_of("https://api.rsclaw.ai/v1/agent/sessions"), Some("https://api.rsclaw.ai"));
        assert_eq!(origin_of("http://localhost:8443/path"), Some("http://localhost:8443"));
        assert_eq!(origin_of("https://host"), Some("https://host"));
        assert_eq!(origin_of("not-a-url"), None);
    }

    #[test]
    fn rewrite_origin_preserves_path_and_query() {
        let url = "https://api.rsclaw.ai/v1/agent/sessions/rs_w7_abc/turn";
        let new = rewrite_origin(url, "https://server.rsclaw.ai:8443");
        assert_eq!(new, "https://server.rsclaw.ai:8443/v1/agent/sessions/rs_w7_abc/turn");
    }

    #[test]
    fn resolve_location_handles_absolute_and_relative() {
        // Absolute URL: returned as-is.
        assert_eq!(
            resolve_location(
                "https://api.rsclaw.ai/v1/agent/sessions",
                "https://server.rsclaw.ai:8443/v1/agent/sessions",
            ),
            Some("https://server.rsclaw.ai:8443/v1/agent/sessions".into()),
        );
        // Absolute path: rebased on caller origin.
        assert_eq!(
            resolve_location("https://api.rsclaw.ai/v1/agent/sessions", "/other/path"),
            Some("https://api.rsclaw.ai/other/path".into()),
        );
        // Empty / relative-to-current-path explicitly unsupported —
        // rsclaw-server doesn't emit those and supporting them invites
        // path-confusion bugs.
        assert_eq!(
            resolve_location("https://api.rsclaw.ai/v1/agent/sessions", ""),
            None,
        );
        assert_eq!(
            resolve_location("https://api.rsclaw.ai/v1/agent/sessions", "other"),
            None,
        );
    }

    #[test]
    fn parse_max_age_extracts_seconds() {
        assert_eq!(parse_max_age(Some("max-age=3600")), Some(Duration::from_secs(3600)));
        assert_eq!(parse_max_age(Some("public, max-age=300, must-revalidate")), Some(Duration::from_secs(300)));
        assert_eq!(parse_max_age(Some("MAX-AGE=120")), Some(Duration::from_secs(120))); // case-insensitive
    }

    #[test]
    fn parse_max_age_returns_none_when_missing() {
        assert_eq!(parse_max_age(None), None);
        assert_eq!(parse_max_age(Some("public")), None);
        assert_eq!(parse_max_age(Some("private, must-revalidate")), None);
    }

    #[test]
    fn parse_max_age_returns_none_on_no_store_or_no_cache() {
        // RFC 7234 says no-store / no-cache forbid the response from
        // being stored regardless of max-age. Honour that override so
        // a server that wants to forcibly disable our redirect cache
        // can do so by adding `no-store` to the 308's Cache-Control.
        assert_eq!(parse_max_age(Some("no-store")), None);
        assert_eq!(parse_max_age(Some("max-age=3600, no-store")), None);
        assert_eq!(parse_max_age(Some("no-cache, max-age=300")), None);
    }

    #[test]
    fn redirect_cache_lookup_returns_target_when_fresh() {
        let mut cache = RedirectCache::default();
        cache.store(
            "https://api.rsclaw.ai".into(),
            "https://server.rsclaw.ai:8443".into(),
            Duration::from_secs(60),
        );
        assert_eq!(
            cache.lookup("https://api.rsclaw.ai"),
            Some("https://server.rsclaw.ai:8443".into()),
        );
        // Miss key → None.
        assert_eq!(cache.lookup("https://other.example.com"), None);
    }

    #[test]
    fn redirect_cache_expires_stale_entries_lazily() {
        let mut cache = RedirectCache::default();
        // Negative TTL → immediately expired. (`Instant::now()` already
        // past the computed `expires_at`.)
        cache.entries.insert(
            "https://api.rsclaw.ai".into(),
            RedirectEntry {
                target_origin: "https://server.rsclaw.ai:8443".into(),
                expires_at: std::time::Instant::now() - Duration::from_secs(1),
            },
        );
        assert!(cache.lookup("https://api.rsclaw.ai").is_none());
        // Entry should be evicted as a side-effect — subsequent
        // lookups don't keep re-walking a dead row.
        assert!(!cache.entries.contains_key("https://api.rsclaw.ai"));
    }

    #[test]
    fn redirect_cache_invalidate_removes_entry() {
        let mut cache = RedirectCache::default();
        cache.store(
            "https://api.rsclaw.ai".into(),
            "https://server.rsclaw.ai:8443".into(),
            Duration::from_secs(3600),
        );
        cache.invalidate("https://api.rsclaw.ai");
        assert!(cache.lookup("https://api.rsclaw.ai").is_none());
    }

    #[test]
    fn provider_resolve_url_returns_origin_when_cache_empty() {
        let provider = RsclawProvider::new("https://api.rsclaw.ai/v1/agent", None);
        assert_eq!(
            provider.resolve_url("/sessions"),
            "https://api.rsclaw.ai/v1/agent/sessions",
        );
    }

    #[test]
    fn provider_resolve_url_rewrites_when_cache_fresh() {
        let provider = RsclawProvider::new("https://api.rsclaw.ai/v1/agent", None);
        if let Ok(mut cache) = provider.redirect_cache.lock() {
            cache.store(
                "https://api.rsclaw.ai".into(),
                "https://server.rsclaw.ai:8443".into(),
                Duration::from_secs(3600),
            );
        }
        assert_eq!(
            provider.resolve_url("/sessions/rs_w7_abc/turn"),
            "https://server.rsclaw.ai:8443/v1/agent/sessions/rs_w7_abc/turn",
        );
    }

    /// Thin shim mirroring `parse_sse_chunk`'s signature so tests don't
    /// have to repeat the lock-wrap boilerplate.
    async fn parse_sse_test(
        chunk: Result<bytes::Bytes>,
        buf: &Arc<tokio::sync::Mutex<String>>,
        rem: &Arc<tokio::sync::Mutex<Vec<u8>>>,
    ) -> Vec<Result<StreamEvent>> {
        parse_sse_chunk(chunk, buf, rem).await
    }

    #[tokio::test]
    async fn parse_sse_chunk_recovers_split_utf8() {
        // SSE delta line carrying "你好"
        // (U+4F60 = E4 BD A0, U+597D = E5 A5 BD), with the byte split
        // landing in the middle of the first character.
        let line_full = b"data: {\"type\":\"delta\",\"content\":\"\xe4\xbd\xa0\xe5\xa5\xbd\"}\n";
        let split = 20;
        let (a, b) = line_full.split_at(split);
        let (b, c) = b.split_at(line_full.len() / 2 - split);
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));

        for piece in [a, b, c] {
            let _ = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(piece)), &buf, &rem).await;
        }
        let evs = parse_sse_test(Ok(bytes::Bytes::from_static(b"")), &buf, &rem).await;

        let texts: Vec<_> = evs
            .into_iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::TextDelta(t)) => Some(t),
                _ => None,
            })
            .collect();
        // Either fully delivered now or already delivered in an earlier piece.
        let all_text: String = texts.into_iter().collect();
        // The final newline-terminated event must produce 你好 verbatim, no U+FFFD.
        assert!(
            !all_text.contains('\u{FFFD}'),
            "expected no replacement char, got {all_text:?}"
        );
    }

    #[tokio::test]
    async fn parse_sse_chunk_advances_past_invalid_utf8_byte() {
        // A stray 0xFF (or any byte that's *invalid as a UTF-8 start*,
        // not just incomplete) MUST be skipped, not pinned in
        // `utf8_remainder`. Without this, every subsequent chunk
        // stitches onto the bad prefix, fails at the same position,
        // and remainder grows unboundedly while no events ever fire —
        // the stream stalls forever on a single bad byte.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));

        // Chunk 1: just an invalid byte. error_len() = Some(1).
        let evs = parse_sse_test(Ok(bytes::Bytes::from_static(b"\xff")), &buf, &rem).await;
        assert!(
            evs.iter().all(|e| e.is_ok()),
            "stray 0xFF must not surface as Err — got {evs:?}"
        );
        {
            let r = rem.lock().await;
            assert!(
                !r.contains(&0xff),
                "0xFF must be advanced past, not pinned in remainder; got {:?}",
                *r
            );
        }

        // Chunk 2: a complete SSE event. The stream must recover and
        // emit it normally — no contamination from the prior bad byte.
        let evs = parse_sse_test(
            Ok(bytes::Bytes::from_static(
                b"data: {\"type\":\"delta\",\"content\":\"hi\"}\n",
            )),
            &buf,
            &rem,
        )
        .await;
        let texts: Vec<_> = evs
            .into_iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::TextDelta(t)) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["hi".to_string()],
            "stream must recover and emit subsequent events after a bad byte"
        );
    }

    #[tokio::test]
    async fn parse_sse_chunk_invalid_byte_does_not_unbounded_grow_remainder() {
        // Regression: feeding the same invalid byte over and over MUST
        // NOT grow `utf8_remainder` linearly. Pre-fix, every call
        // appended the bad byte and re-saved the entire stitched buffer;
        // 1000 chunks → 1000-byte remainder → eventual OOM in long
        // streams. Post-fix the remainder stays empty after each call.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        for _ in 0..50 {
            let _ = parse_sse_test(Ok(bytes::Bytes::from_static(b"\xff")), &buf, &rem).await;
        }
        let r = rem.lock().await;
        assert!(
            r.len() <= 3,
            "remainder must not accumulate invalid bytes (cap 3 for trailing incomplete UTF-8); got {} bytes",
            r.len()
        );
    }

    #[tokio::test]
    async fn parse_sse_chunk_skips_empty_data_payload() {
        // Empty `data:` lines (a heartbeat shape some proxies emit when
        // translating `:keepalive` comments) MUST NOT surface as Err in
        // the stream — the runtime propagates Err with `?` and would
        // kill an otherwise-healthy turn. Mix an empty line with a real
        // event and assert: only the real text delta fires, no Err.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data:\ndata: {\"type\":\"delta\",\"content\":\"hi\"}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let mut texts: Vec<String> = Vec::new();
        for e in evs {
            match e {
                Ok(StreamEvent::TextDelta(t)) => texts.push(t),
                Err(err) => panic!("empty data: must not surface as Err — got {err}"),
                _ => {}
            }
        }
        assert_eq!(texts, vec!["hi".to_string()]);
    }

    #[tokio::test]
    async fn parse_sse_chunk_skips_data_with_only_spaces() {
        // `data:    \n` (whitespace-only after the colon) trims to "" via
        // `trim_start_matches(' ')` and lands in the same empty-skip path.
        // Verify the same: no Err, no spurious event.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data:    \n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        for e in evs {
            if let Err(err) = e {
                panic!("whitespace-only data: must not surface as Err — got {err}");
            }
        }
    }

    #[tokio::test]
    async fn parse_sse_chunk_accepts_data_without_leading_space() {
        // SSE field syntax allows the space after the colon to be
        // omitted; rsclaw-server (or any node that routes through
        // hyper / nginx with comp-stripping middleware) may emit
        // `data:{...}` without a space. Both forms must parse.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data:{\"type\":\"delta\",\"content\":\"hi\"}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let texts: Vec<_> = evs
            .into_iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::TextDelta(t)) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hi".to_string()]);
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_delta_emits_text() {
        // Happy-path text fragment. Worker emits one of these per
        // generated token on every native lane.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"delta\",\"content\":\"hello\"}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let texts: Vec<String> = evs
            .into_iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::TextDelta(s)) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hello".to_string()]);
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_thinking_emits_reasoning() {
        // Reasoning-model lane: `{type:"thinking",content:"..."}`
        // maps to ReasoningDelta so the agent runtime can stash it
        // separately from user-visible TextDelta.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"thinking\",\"content\":\"step 1: parse\"}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let reasonings: Vec<String> = evs
            .into_iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::ReasoningDelta(s)) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(reasonings, vec!["step 1: parse".to_string()]);
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_tool_call_emits_whole_frame() {
        // Native tool_call arrives complete in ONE frame — no
        // accumulation across deltas (unlike Anthropic's
        // input_json_delta streaming). id, name, input all materialise
        // together.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = br#"data: {"type":"tool_call","id":"call_42","name":"read_file","input":{"path":"x.rs"}}
"#;
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let (id, name, input) = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::ToolCall { id, name, input }) => Some((id, name, input)),
                _ => None,
            })
            .expect("expected one ToolCall event");
        assert_eq!(id, "call_42");
        assert_eq!(name, "read_file");
        assert_eq!(input.get("path").and_then(Value::as_str), Some("x.rs"));
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_tool_call_missing_input_defaults_empty_object() {
        // §4.2 says: emit ToolCall with input = {} when the worker
        // omits or sends a non-object input. Downstream consumers
        // call `.as_object()` directly without a Null match.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"tool_call\",\"id\":\"c\",\"name\":\"get_time\"}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let input = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::ToolCall { input, .. }) => Some(input),
                _ => None,
            })
            .expect("expected one ToolCall event");
        assert!(
            input.as_object().is_some_and(|m| m.is_empty()),
            "missing input must default to empty object, got {input:?}"
        );
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_done_emits_done_with_usage() {
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"done\",\"finish_reason\":\"end_turn\",\"usage\":{\"input_tokens\":11,\"output_tokens\":22}}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let mut saw_done = false;
        for e in evs {
            if let Ok(StreamEvent::Done { usage }) = e {
                let u = usage.expect("usage should be populated");
                assert_eq!(u.input, 11);
                assert_eq!(u.output, 22);
                saw_done = true;
            }
        }
        assert!(saw_done, "expected Done from native done frame");
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_done_without_usage() {
        // Server may omit usage on early termination — Done must still fire.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"done\",\"finish_reason\":\"end_turn\"}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let mut saw_done = false;
        for e in evs {
            if let Ok(StreamEvent::Done { usage }) = e {
                assert!(usage.is_none());
                saw_done = true;
            }
        }
        assert!(saw_done, "expected Done event even without usage");
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_done_usage_field_name_fallback() {
        // §4.3: lanes differ on usage field names. Each side must try
        //   input_tokens || prompt_tokens || input
        //   output_tokens || completion_tokens || output
        // and default missing to 0 rather than dropping the whole
        // usage object.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"done\",\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":13}}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let u = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::Done { usage }) => usage,
                _ => None,
            })
            .expect("expected Done with usage");
        assert_eq!(u.input, 7);
        assert_eq!(u.output, 13);
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_done_partial_usage_keeps_present_side() {
        // Pre-fix the `?` short-circuit nuked the entire TokenUsage on
        // a single missing field. Default each side to 0 so the half
        // we DID get is preserved.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"done\",\"usage\":{\"input_tokens\":17}}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let usage = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::Done { usage }) => Some(usage),
                _ => None,
            })
            .expect("expected one Done event")
            .expect("usage should survive partial fields");
        assert_eq!(usage.input, 17);
        assert_eq!(usage.output, 0);
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_done_null_usage_is_none() {
        // `"usage": null` must collapse to None, not Some(0,0) — a
        // phantom zero-token turn would dilute accounting averages and
        // mask buggy worker builds that drop the field.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"done\",\"usage\":null}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let mut saw_done = false;
        for e in evs {
            if let Ok(StreamEvent::Done { usage }) = e {
                assert!(usage.is_none(), "null usage must collapse to None, got {usage:?}");
                saw_done = true;
            }
        }
        assert!(saw_done, "expected Done event with usage=None");
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_done_non_object_usage_is_none() {
        // Malformed `"usage": [1,2]` must NOT yield Some(TokenUsage{0,0}).
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"done\",\"usage\":[1,2,3]}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let mut saw_done = false;
        for e in evs {
            if let Ok(StreamEvent::Done { usage }) = e {
                assert!(usage.is_none(), "non-object usage must collapse to None");
                saw_done = true;
            }
        }
        assert!(saw_done, "expected Done event");
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_error_preserves_code_and_message() {
        // §4.2: error frame is `{type:"error", error:{code, message}}`.
        // Both fields must survive into StreamEvent::Error.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = br#"data: {"type":"error","error":{"code":"slot_evicted","message":"slot was reclaimed mid-decode"}}
"#;
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let msg = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::Error(m)) => Some(m),
                _ => None,
            })
            .expect("expected one Error event");
        assert!(msg.contains("slot_evicted"), "missing code: {msg}");
        assert!(msg.contains("slot was reclaimed mid-decode"), "missing message: {msg}");
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_error_code_missing_keeps_message() {
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = br#"data: {"type":"error","error":{"message":"upstream hung up"}}
"#;
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let msg = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::Error(m)) => Some(m),
                _ => None,
            })
            .expect("expected one Error event");
        assert!(msg.contains("upstream hung up"), "missing message: {msg}");
        assert!(!msg.contains("[]"), "empty-code marker leaked: {msg}");
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_error_message_missing_keeps_code() {
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = br#"data: {"type":"error","error":{"code":"version_drift"}}
"#;
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let msg = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::Error(m)) => Some(m),
                _ => None,
            })
            .expect("expected one Error event");
        assert!(msg.contains("version_drift"), "missing code: {msg}");
        assert!(!msg.ends_with(": "), "trailing empty-message leaked: {msg}");
    }

    #[tokio::test]
    async fn parse_sse_chunk_native_error_uses_default_when_both_missing() {
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"error\",\"error\":{}}\n";
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let msg = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::Error(m)) => Some(m),
                _ => None,
            })
            .expect("expected one Error event");
        assert_eq!(msg, "rsclaw stream error");
    }

    #[tokio::test]
    async fn parse_sse_chunk_unknown_type_ignored_for_forward_compat() {
        // §4.2 forward-compat rule: unknown types (e.g. future
        // `cache_hit_summary` frames) must be silently ignored.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let frames = br#"data: {"type":"cache_hit_summary","hits":42}
data: {"type":"delta","content":"hi"}
"#;
        let evs = parse_sse_test(Ok(bytes::Bytes::copy_from_slice(frames)), &buf, &rem).await;
        let texts: Vec<String> = evs
            .into_iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::TextDelta(s)) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hi".to_string()]);
    }

    #[test]
    fn turn_headers_timeout_is_bounded_and_finite() {
        // Sanity-check the constant: streaming turns rely on this for
        // wedged-server detection. Too short would cause spurious
        // failures on a slow TLS handshake; too long would let a dead
        // server hang the runtime indefinitely.
        let s = TURN_HEADERS_TIMEOUT.as_secs();
        assert!((30..=120).contains(&s), "TURN_HEADERS_TIMEOUT={s}s out of range");
    }

    #[test]
    fn auth_header_omits_when_bearer_is_none_or_empty() {
        // `RSCLAW_KEY=""` (env var set but blank) flows in as
        // `Some("")` from `std::env::var(...).ok()` — sending
        // `Authorization: Bearer ` would be rejected by stricter
        // proxies. Treat None and Some("") as the same "no auth"
        // signal at the wire boundary.
        let p = RsclawProvider::new("http://x", None);
        assert!(p.auth_header().is_none());
        let p = RsclawProvider::new("http://x", Some(String::new()));
        assert!(p.auth_header().is_none());
    }

    #[test]
    fn auth_header_emits_bearer_when_populated() {
        let p = RsclawProvider::new("http://x", Some("sk-abc".into()));
        let (k, v) = p.auth_header().expect("bearer set");
        assert_eq!(k, "authorization");
        assert_eq!(v, "Bearer sk-abc");
    }

    #[test]
    fn ctor_trims_whitespace_from_base_url_and_bearer() {
        // dotenv-loaded env vars routinely carry a trailing newline —
        // `RSCLAW_KEY=sk-abc\n` round-trips into the provider as
        // `Some("sk-abc\n")`. reqwest rejects HTTP header values
        // containing `\n` (RFC 7230), so without trimming every signed
        // request fails before leaving the process. Same hazard for
        // base_url where leading/trailing whitespace breaks URL parse.
        let p = RsclawProvider::new(
            "  http://x:8090/v1/agent/  ",
            Some("  sk-abc\n  ".into()),
        );
        assert_eq!(p.base_url, "http://x:8090/v1/agent");
        let (k, v) = p.auth_header().expect("bearer survived trim");
        assert_eq!(k, "authorization");
        assert_eq!(v, "Bearer sk-abc");
    }

    #[test]
    fn ctor_blank_after_trim_bearer_becomes_none() {
        // `RSCLAW_KEY="   "` (whitespace-only) MUST NOT survive as
        // `Some("   ")` — that would emit `Authorization: Bearer    `
        // which stricter proxies reject the same way they reject the
        // empty-string form covered by `auth_header_omits_when_*`.
        let p = RsclawProvider::new("http://x", Some("   \n\t".into()));
        assert!(p.bearer.is_none());
        assert!(p.auth_header().is_none());
    }

    #[test]
    fn is_session_evicted_recognizes_session_not_found() {
        let body = r#"{"error":{"code":"session_not_found","detail":"slot evicted"}}"#;
        assert!(is_session_evicted(StatusCode::NOT_FOUND, body));
    }

    #[test]
    fn is_session_evicted_rejects_404_with_other_code() {
        // A 404 from a misrouted request (e.g. wrong path → CDN 404
        // page or `404 unknown_version` from /sessions/replay) MUST NOT
        // be treated as a session eviction. Earlier code blindly
        // short-circuited any 404 to SessionNotFound, which would loop
        // forever in replay; the unified `is_session_evicted` check
        // requires the body to confirm the eviction code.
        let body = r#"{"error":{"code":"unknown_version","detail":"v not registered"}}"#;
        assert!(!is_session_evicted(StatusCode::NOT_FOUND, body));
        assert!(!is_session_evicted(StatusCode::NOT_FOUND, ""));
        assert!(!is_session_evicted(
            StatusCode::NOT_FOUND,
            "<html>not found</html>",
        ));
    }

    #[test]
    fn is_session_evicted_recognizes_version_drift() {
        let body = r#"{"error":{"code":"version_drift","detail":"node has been upgraded"}}"#;
        assert!(is_session_evicted(StatusCode::CONFLICT, body));
    }

    #[test]
    fn is_session_evicted_recognizes_backend_unavailable() {
        let body = r#"{"error":{"code":"backend_unavailable","detail":"heartbeat timeout"}}"#;
        assert!(is_session_evicted(StatusCode::SERVICE_UNAVAILABLE, body));
    }

    #[test]
    fn is_session_evicted_excludes_no_backend_available() {
        // Capacity exhaustion — replay won't help, must bail.
        let body = r#"{"error":{"code":"no_backend_available","detail":"all GPUs saturated"}}"#;
        assert!(!is_session_evicted(StatusCode::SERVICE_UNAVAILABLE, body));
    }

    #[test]
    fn is_session_evicted_rejects_status_code_mismatch() {
        // Right code, wrong status — don't recover.
        let body = r#"{"error":{"code":"version_drift","detail":"x"}}"#;
        assert!(!is_session_evicted(StatusCode::SERVICE_UNAVAILABLE, body));
        let body = r#"{"error":{"code":"backend_unavailable","detail":"x"}}"#;
        assert!(!is_session_evicted(StatusCode::CONFLICT, body));
    }

    #[test]
    fn is_session_evicted_rejects_malformed_body() {
        assert!(!is_session_evicted(StatusCode::CONFLICT, ""));
        assert!(!is_session_evicted(StatusCode::CONFLICT, "not json"));
        assert!(!is_session_evicted(
            StatusCode::CONFLICT,
            r#"{"code":"version_drift"}"#,
        ));
    }

    fn req_with(messages: Vec<Message>, mode: u8, key: Option<&str>) -> LlmRequest {
        LlmRequest {
            model: "2026.5.15".into(),
            messages,
            system: Some("you are an agent".into()),
            kv_cache_mode: mode,
            session_key: key.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn canonical_value_sorts_object_keys_alphabetically() {
        // `preserve_order` keeps the IndexMap insertion order; passing
        // a JSON literal in a non-alphabetical order verifies the
        // canonical pass reorders keys.
        let input = json!({
            "z_last": 1,
            "a_first": 2,
            "m_mid": 3,
        });
        let canon = to_canonical_value(input);
        let serialized = serde_json::to_string(&canon).unwrap();
        // BTreeMap-sourced canonical output must emit keys alphabetically.
        assert_eq!(serialized, r#"{"a_first":2,"m_mid":3,"z_last":1}"#);
    }

    #[test]
    fn canonical_value_recurses_into_nested_objects() {
        // The whole point of canonicalization is that nested schema
        // bodies (input_schema.properties.{...}) also get a stable
        // order — that's where actual tool parameter HashMaps surface.
        let input = json!({
            "outer_b": {"y": 1, "x": 2},
            "outer_a": {"inner": {"z": 0, "a": 1}},
        });
        let canon = to_canonical_value(input);
        let s = serde_json::to_string(&canon).unwrap();
        assert_eq!(
            s,
            r#"{"outer_a":{"inner":{"a":1,"z":0}},"outer_b":{"x":2,"y":1}}"#
        );
    }

    #[test]
    fn canonical_value_preserves_array_order() {
        // Arrays are positional, not associative — order is meaningful
        // (e.g. tool ordering, message history). Don't touch.
        let input = json!([3, 1, 2, {"b": 1, "a": 2}]);
        let canon = to_canonical_value(input);
        let s = serde_json::to_string(&canon).unwrap();
        assert_eq!(s, r#"[3,1,2,{"a":2,"b":1}]"#);
    }

    #[test]
    fn canonical_value_is_idempotent() {
        // Running canonicalization twice must produce identical output —
        // worker prefix hashing depends on byte equality across runs,
        // so the operation must be a fixed point.
        let input = json!({
            "tools": [{
                "name": "search",
                "input_schema": {
                    "type": "object",
                    "properties": {"q": {"type": "string"}, "k": {"type": "integer"}},
                    "required": ["q"]
                }
            }]
        });
        let once = to_canonical_value(input.clone());
        let twice = to_canonical_value(once.clone());
        assert_eq!(
            serde_json::to_string(&once).unwrap(),
            serde_json::to_string(&twice).unwrap()
        );
    }

    #[test]
    fn canonical_value_byte_stable_across_input_orderings() {
        // The smoking-gun assertion: two logically-identical JSON
        // values constructed with different insertion orders MUST
        // serialize to the same bytes after canonicalization. This
        // is the exact property the worker's prefix hash depends on.
        let a = json!({
            "z": [{"b": 1, "a": 2}],
            "a": {"inner_z": 1, "inner_a": 2},
        });
        let b = json!({
            "a": {"inner_a": 2, "inner_z": 1},
            "z": [{"a": 2, "b": 1}],
        });
        let canon_a = to_canonical_value(a);
        let canon_b = to_canonical_value(b);
        assert_eq!(
            serde_json::to_string(&canon_a).unwrap(),
            serde_json::to_string(&canon_b).unwrap()
        );
    }

    #[test]
    fn split_request_dynamic_tools_are_canonical() {
        // End-to-end: feed a tool whose input_schema has keys in a
        // non-alphabetical order, verify the dynamic_tools entry the
        // provider would put on the wire is in alphabetical order.
        let mut req = req_with(vec![], 2, Some("k"));
        req.tools = vec![crate::provider::ToolDef {
            name: "search".into(),
            description: "look stuff up".into(),
            parameters: json!({
                "type": "object",
                "required": ["q"],
                "properties": {
                    "q": {"type": "string"},
                    "k": {"type": "integer"},
                }
            }),
        }];
        let split = split_request(&req, RSCLAW_DEFAULT_PREFIX_ID).unwrap();
        assert_eq!(split.dynamic_tools.len(), 1);
        let serialized = serde_json::to_string(&split.dynamic_tools[0]).unwrap();
        // Top level keys: description, input_schema, name (alphabetical).
        // input_schema body: properties, required, type. properties body: k, q.
        // Property bodies: type only (single key — order irrelevant).
        assert_eq!(
            serialized,
            concat!(
                r#"{"description":"look stuff up","input_schema":"#,
                r#"{"properties":{"k":{"type":"integer"},"q":{"type":"string"}},"#,
                r#""required":["q"],"type":"object"},"name":"search"}"#
            )
        );
    }

    #[test]
    fn split_request_uses_provided_prefix_id_verbatim() {
        // prefix_id is config-driven; split_request passes it through
        // without inspecting req.model. Two distinct model strings on
        // the same request must yield the SAME wire prefix_id when the
        // caller supplied the same value.
        let mut req = req_with(vec![], 2, Some("k"));
        req.model = "qwen3-235b".into();
        let split = split_request(&req, RSCLAW_DEFAULT_PREFIX_ID).unwrap();
        assert_eq!(split.prefix_id, "rsclaw/2026.5.18");

        req.model = "myorg/qwen3-235b".into();
        let split2 = split_request(&req, RSCLAW_DEFAULT_PREFIX_ID).unwrap();
        assert_eq!(split2.prefix_id, "rsclaw/2026.5.18");
    }

    #[test]
    fn split_request_honours_custom_prefix_id_override() {
        // Provider configured with a non-default prefix_id (e.g. a
        // tenant's private namespace) — split_request forwards the
        // override verbatim, independent of req.model.
        let mut req = req_with(vec![], 2, Some("k"));
        req.model = "qwen3-235b".into();
        let split = split_request(&req, "myorg/2026.5.15").unwrap();
        assert_eq!(split.prefix_id, "myorg/2026.5.15");
    }

    #[test]
    fn with_prefix_id_overrides_default_and_ignores_blank() {
        // Builder swaps in the override; whitespace-only / empty input
        // is rejected so a misconfigured config file can't produce a
        // §2.10.1-invalid wire value.
        let p = RsclawProvider::new("http://x", None);
        assert_eq!(p.prefix_id, RSCLAW_DEFAULT_PREFIX_ID);

        let p = RsclawProvider::new("http://x", None).with_prefix_id("tenant/2026.6.1");
        assert_eq!(p.prefix_id, "tenant/2026.6.1");

        let p = RsclawProvider::new("http://x", None).with_prefix_id("   \n  ");
        assert_eq!(p.prefix_id, RSCLAW_DEFAULT_PREFIX_ID);
    }

    #[test]
    fn with_prefix_id_rejects_invalid_slash_count() {
        // §2.10.1 mandates exactly one '/' separator. A config typo with
        // zero slashes (e.g. "rsclaw-2026.5.15") or two+ ("foo/bar/baz")
        // would survive boot and only fail on the first wire call, which
        // is annoying to debug. Validate at the builder so the override
        // is dropped early and we boot with the safe default.
        let default = RSCLAW_DEFAULT_PREFIX_ID;

        let p = RsclawProvider::new("http://x", None).with_prefix_id("rsclaw-2026.5.15");
        assert_eq!(p.prefix_id, default, "no slash → reject");

        let p = RsclawProvider::new("http://x", None).with_prefix_id("foo/bar/baz");
        assert_eq!(p.prefix_id, default, "two slashes → reject");

        // Surrounding whitespace gets trimmed before validation — a
        // valid value with stray dotenv newline still works.
        let p = RsclawProvider::new("http://x", None).with_prefix_id("  tenant/v1\n");
        assert_eq!(p.prefix_id, "tenant/v1", "trim before count");
    }

    #[test]
    fn split_request_orders_builtin_before_user_tools_when_split_present() {
        // With `system_shared` populated, the runtime is in real-split
        // mode: tools are ordered [builtin..., user...] inside
        // `dynamic_prefix.tools` so the chat-template-rendered byte
        // prefix stays stable across every client of this RsClaw
        // version up to the per-client tool boundary. Top-level
        // `user_tools` is GONE in the post-rename protocol — verified
        // by rsclaw-server's own
        // `v1 top-level user_tools must not be sent` test.
        let mut req = req_with(vec![], 2, Some("k"));
        req.system_shared = Some("<shared system>".into());
        req.user_system = Some("<user suffix>".into());
        // Push user-tool first to prove the split sorts it after the
        // builtin regardless of input order.
        req.tools.push(ToolDef {
            name: "search".into(), // not in BUILTIN_TOOL_NAMES
            description: "search the web".into(),
            parameters: json!({"type":"object","properties":{}}),
        });
        req.tools.push(ToolDef {
            name: "memory".into(), // builtin
            description: "memory tool".into(),
            parameters: json!({"type":"object","properties":{}}),
        });
        let split = split_request(&req, RSCLAW_DEFAULT_PREFIX_ID).unwrap();
        assert_eq!(split.dynamic_tools.len(), 2);
        assert_eq!(
            split.dynamic_tools[0]["name"], "memory",
            "builtin must sort before user tool"
        );
        assert_eq!(split.dynamic_tools[1]["name"], "search");
        assert_eq!(split.dynamic_system, "<shared system>");
        assert_eq!(split.dynamic_user_system, "<user suffix>");
    }

    #[test]
    fn split_request_collapses_to_dynamic_when_no_split() {
        // Internal sessions / non-runtime callers don't populate the
        // shared/user split. Everything collapses into `dynamic_prefix`
        // in input order — per-client cache sharing is forfeit but
        // that's the no-regression baseline.
        let mut req = req_with(vec![], 2, Some("k"));
        req.tools.push(ToolDef {
            name: "search".into(),
            description: "search".into(),
            parameters: json!({"type":"object"}),
        });
        let split = split_request(&req, RSCLAW_DEFAULT_PREFIX_ID).unwrap();
        assert_eq!(split.dynamic_tools.len(), 1);
        assert_eq!(split.dynamic_tools[0]["name"], "search");
        assert_eq!(split.dynamic_system, "you are an agent");
        assert_eq!(split.dynamic_user_system, "");
    }

    #[test]
    fn create_session_req_serialises_post_rename_shape() {
        // Matches the wire body rsclaw-server's backend/rsclaw_llm.rs
        // tests assert: `prefix_id` + `dynamic_prefix{system,tools,
        // user_system}` + `options`, and explicitly NO top-level
        // `user_tools` / `rsclaw_version` / `user_suffix` /
        // `user_system` / `plugins_system` / `skills_system`. The
        // `user_suffix` legacy name (and `user_system` accidentally
        // promoted to top-level) is asserted absent so a refactor that
        // re-emits either at top-level gets caught by the test.
        let mut req = req_with(vec![], 2, Some("k"));
        req.system_shared = Some("<sys>".into());
        req.user_system = Some("<suf>".into());
        req.tools.push(ToolDef {
            name: "memory".into(),
            description: "memory tool".into(),
            parameters: json!({"type":"object"}),
        });
        let split = split_request(&req, RSCLAW_DEFAULT_PREFIX_ID).unwrap();
        let body = CreateSessionReq {
            prefix_id: &split.prefix_id,
            model: &split.model,
            dynamic_prefix: DynamicPrefixWire {
                system: split.dynamic_system,
                tools: &split.dynamic_tools,
                user_system: split.dynamic_user_system,
            },
            options: Some(split.options.clone()),
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["prefix_id"], "rsclaw/2026.5.18");
        assert_eq!(v["dynamic_prefix"]["system"], "<sys>");
        assert_eq!(v["dynamic_prefix"]["user_system"], "<suf>");
        assert_eq!(v["dynamic_prefix"]["tools"][0]["name"], "memory");
        assert!(v.get("user_tools").is_none(), "post-rename body must omit top-level user_tools");
        assert!(v.get("rsclaw_version").is_none(), "rsclaw_version is the pre-rename name; never send");
        assert!(v.get("user_suffix").is_none(), "user_suffix is the legacy name; never send (top-level or otherwise)");
        assert!(v.get("user_system").is_none(), "user_system lives inside dynamic_prefix, never at top-level");
        assert!(v.get("plugins_system").is_none(), "pre-rename field; folded into dynamic_prefix.system");
        assert!(v.get("skills_system").is_none(), "pre-rename field; folded into dynamic_prefix.system");
    }

    #[test]
    fn history_for_replay_drops_trailing_delta() {
        let m = |role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let msgs = vec![
            m(Role::User, "hi"),
            m(Role::Assistant, "yo"),
            m(Role::User, "again"),
        ];
        let slice = history_for_replay(&msgs);
        assert_eq!(slice.len(), 2);
        assert!(matches!(slice[0].role, Role::User));
        assert!(matches!(slice[1].role, Role::Assistant));
    }

    #[test]
    fn history_for_replay_handles_empty_and_singleton() {
        let empty: Vec<Message> = Vec::new();
        assert!(history_for_replay(&empty).is_empty());
        let one = vec![Message {
            role: Role::User,
            content: MessageContent::Text("solo".into()),
        }];
        assert!(history_for_replay(&one).is_empty());
    }

    #[test]
    fn history_for_replay_drops_all_consecutive_trailing_tools() {
        // Parallel-tool case: assistant emits N tool_use blocks, runtime
        // queues N consecutive Role::Tool messages, from_request folds
        // them into a single Tools delta. history_for_replay must drop
        // ALL N — dropping just one leaves N-1 tool_results in history,
        // server replays them into KV, then turn() re-sends them as the
        // delta, hydrating duplicates.
        let m = |role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let tool = |id: &str| Message {
            role: Role::Tool,
            content: MessageContent::Parts(vec![ContentPart::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: None,
            }]),
        };
        let msgs = vec![
            m(Role::User, "do all three"),
            m(Role::Assistant, "calling tools"),
            tool("toolu_1"),
            tool("toolu_2"),
            tool("toolu_3"),
        ];
        let slice = history_for_replay(&msgs);
        assert_eq!(slice.len(), 2);
        assert!(matches!(slice[0].role, Role::User));
        assert!(matches!(slice[1].role, Role::Assistant));
    }

    #[test]
    fn history_for_replay_keeps_earlier_tool_messages() {
        // Sequential-tool case across a multi-iteration turn:
        // [..., User, Asst, Tool, Asst, Tool, Asst, Tool] — only the
        // FINAL contiguous Tool run belongs to the current step's delta;
        // earlier Tool messages are part of completed sub-iterations and
        // stay in history.
        let m = |role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let tool = |id: &str| Message {
            role: Role::Tool,
            content: MessageContent::Parts(vec![ContentPart::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: None,
            }]),
        };
        let msgs = vec![
            m(Role::User, "go"),
            m(Role::Assistant, "step1"),
            tool("a"),
            m(Role::Assistant, "step2"),
            tool("b"),
        ];
        let slice = history_for_replay(&msgs);
        assert_eq!(slice.len(), 4);
        // The earlier Tool stays in history.
        assert!(matches!(slice[2].role, Role::Tool));
        // Trailing Tool dropped.
        assert!(matches!(slice[3].role, Role::Assistant));
    }

    #[test]
    fn serialize_replay_history_coalesces_parallel_tools() {
        // Assistant called 3 tools in parallel → runtime queued 3 Tool
        // messages. In replay history they MUST collapse into one
        // user-role entry whose content[] carries all three tool_results,
        // matching the protocol §2.2 example shape.
        let mk_tool = |id: &str, body: &str| Message {
            role: Role::Tool,
            content: MessageContent::Parts(vec![ContentPart::ToolResult {
                tool_use_id: id.into(),
                content: body.into(),
                is_error: None,
            }]),
        };
        let user = Message {
            role: Role::User,
            content: MessageContent::Text("go".into()),
        };
        let asst = Message {
            role: Role::Assistant,
            content: MessageContent::Text("calling tools".into()),
        };
        let ta = mk_tool("a", "ra");
        let tb = mk_tool("b", "rb");
        let tc = mk_tool("c", "rc");
        let msgs = vec![&user, &asst, &ta, &tb, &tc];
        let out = serialize_replay_history(&msgs);
        assert_eq!(out.len(), 3, "user + assistant + 1 coalesced tool entry: {out:?}");
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[2]["role"], "user");
        let parts = out[2]["content"].as_array().expect("content array");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0]["tool_use_id"], "a");
        assert_eq!(parts[1]["tool_use_id"], "b");
        assert_eq!(parts[2]["tool_use_id"], "c");
        for p in parts {
            assert_eq!(p["type"], "tool_result");
        }
    }

    #[test]
    fn serialize_replay_history_keeps_separated_tool_runs_separate() {
        // Sequential-tool sub-iterations: Tool, Asst, Tool → two distinct
        // user-role entries (one per tool run), with the assistant block
        // between them.
        let mk_tool = |id: &str| Message {
            role: Role::Tool,
            content: MessageContent::Parts(vec![ContentPart::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: None,
            }]),
        };
        let asst = Message {
            role: Role::Assistant,
            content: MessageContent::Text("step".into()),
        };
        let ta = mk_tool("a");
        let tb = mk_tool("b");
        let msgs = vec![&ta, &asst, &tb];
        let out = serialize_replay_history(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"][0]["tool_use_id"], "a");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"][0]["tool_use_id"], "b");
    }

    #[test]
    fn serialize_replay_history_drops_tool_run_with_no_tool_result_parts() {
        // Defensive: a stray Role::Tool message carrying non-ToolResult
        // parts (Text/Image/etc) should not produce an empty user-role
        // entry — that would be `{"role":"user","content":[]}`, which
        // some chat templates reject.
        let bad = Message {
            role: Role::Tool,
            content: MessageContent::Parts(vec![ContentPart::Text { text: "noise".into() }]),
        };
        let user = Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
        };
        let msgs = vec![&user, &bad];
        let out = serialize_replay_history(&msgs);
        assert_eq!(out.len(), 1, "only the User survives: {out:?}");
        assert_eq!(out[0]["role"], "user");
    }

    #[test]
    fn turn_delta_user_text() {
        let req = req_with(
            vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
            }],
            2,
            Some("k"),
        );
        let delta = TurnDelta::from_request(&req).unwrap();
        let body = serde_json::to_value(&delta).unwrap();
        assert_eq!(body["user_message"], "hello");
    }

    #[test]
    fn turn_delta_user_text_empty_bails() {
        let req = req_with(
            vec![Message {
                role: Role::User,
                content: MessageContent::Text(String::new()),
            }],
            2,
            Some("k"),
        );
        let err = TurnDelta::from_request(&req).unwrap_err().to_string();
        assert!(err.contains("no usable content"), "got: {err}");
    }

    #[test]
    fn turn_delta_user_parts_with_only_empty_text_bails() {
        let req = req_with(
            vec![Message {
                role: Role::User,
                content: MessageContent::Parts(vec![
                    ContentPart::Text { text: String::new() },
                    ContentPart::Text { text: String::new() },
                ]),
            }],
            2,
            Some("k"),
        );
        let err = TurnDelta::from_request(&req).unwrap_err().to_string();
        assert!(err.contains("no usable content"), "got: {err}");
    }

    #[test]
    fn turn_delta_user_parts_concatenates_text_fragments() {
        let req = req_with(
            vec![Message {
                role: Role::User,
                content: MessageContent::Parts(vec![
                    ContentPart::Text { text: "hello ".into() },
                    ContentPart::Text { text: "world".into() },
                ]),
            }],
            2,
            Some("k"),
        );
        let delta = TurnDelta::from_request(&req).unwrap();
        let body = serde_json::to_value(&delta).unwrap();
        assert_eq!(body["user_message"], "hello world");
    }

    #[test]
    fn turn_delta_tool_results() {
        let req = req_with(
            vec![Message {
                role: Role::Tool,
                content: MessageContent::Parts(vec![ContentPart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "ok".into(),
                    is_error: None,
                }]),
            }],
            2,
            Some("k"),
        );
        let delta = TurnDelta::from_request(&req).unwrap();
        let body = serde_json::to_value(&delta).unwrap();
        assert_eq!(body["tool_results"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn lookup_and_bump_evicts_on_history_shrink() {
        let provider = RsclawProvider::new("http://x", None);
        provider.store(
            "k",
            SessionEntry {
                session_id: "rs_w7_abc".into(),
                prefix_id: "rsclaw/2026.5.18".into(),
                last_seen_msgs_len: 12,
            },
        );
        // Same len → cached entry returned, last_seen unchanged.
        assert!(provider.lookup_and_bump("k", "rsclaw/2026.5.18", 12).is_some());
        // Growth → bumped, returned.
        assert!(provider.lookup_and_bump("k", "rsclaw/2026.5.18", 14).is_some());
        // Shrink (compaction trimmed history) → None, caller re-hydrates.
        assert!(provider.lookup_and_bump("k", "rsclaw/2026.5.18", 8).is_none());
        // Version drift → None even if len matches.
        assert!(provider.lookup_and_bump("k", "rsclaw/2026.5.6", 14).is_none());
        // Missing key → None.
        assert!(provider.lookup_and_bump("missing", "rsclaw/2026.5.18", 14).is_none());
    }

    #[test]
    fn evict_if_oversized_culls_to_half_cap_when_over() {
        // Construct a HashMap larger than MAX_SESSIONS to verify the
        // batched eviction policy actually drops entries (not all, not
        // none) when the cache exceeds the cap. Cap is 10_000 so use a
        // synthetic over-cap fill.
        let mut map: HashMap<String, SessionEntry> = HashMap::new();
        let total = MAX_SESSIONS + 100;
        for i in 0..total {
            map.insert(
                format!("k{i}"),
                SessionEntry {
                    session_id: format!("rs_w_{i}"),
                    prefix_id: "rsclaw/test".into(),
                    last_seen_msgs_len: 1,
                },
            );
        }
        evict_if_oversized(&mut map);
        // After culling we expect ~MAX_SESSIONS/2 retained: the formula
        // drops (total - MAX_SESSIONS/2) entries.
        assert_eq!(map.len(), MAX_SESSIONS / 2);
    }

    #[test]
    fn evict_if_oversized_no_op_when_under_cap() {
        // Below the cap the function must NOT touch the map — eviction
        // is purely a memory-safety measure, not a routine GC.
        let mut map: HashMap<String, SessionEntry> = HashMap::new();
        for i in 0..100 {
            map.insert(
                format!("k{i}"),
                SessionEntry {
                    session_id: format!("rs_{i}"),
                    prefix_id: "rsclaw/test".into(),
                    last_seen_msgs_len: 1,
                },
            );
        }
        evict_if_oversized(&mut map);
        assert_eq!(map.len(), 100);
    }

    #[tokio::test]
    async fn invalidate_on_error_evicts_session_on_first_err() {
        // Wrap a stream that yields one Ok then an Err; the wrapper
        // must remove the session entry when the Err lands. Subsequent
        // Err items don't re-evict (idempotency by `errored` flag).
        let provider = RsclawProvider::new("http://x", None);
        provider.store(
            "session-key",
            SessionEntry {
                session_id: "rs_w7_xyz".into(),
                prefix_id: "rsclaw/test".into(),
                last_seen_msgs_len: 5,
            },
        );
        let inner: LlmStream = Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::TextDelta("hi".into())),
            Err(anyhow::anyhow!("boom")),
        ]));
        let wrapped = invalidate_on_error(
            inner,
            Arc::clone(&provider.sessions),
            "session-key".to_owned(),
        );
        let collected: Vec<_> = wrapped.collect().await;
        assert_eq!(collected.len(), 2);
        assert!(matches!(collected[0], Ok(StreamEvent::TextDelta(_))));
        assert!(collected[1].is_err());
        // Session must be gone after the error item passed through.
        assert!(provider.lock_sessions().get("session-key").is_none());
    }

    #[tokio::test]
    async fn invalidate_on_error_evicts_on_stream_event_error() {
        // Protocol §2.3 `error` events surface as `Ok(StreamEvent::Error)`
        // — these MUST also force eviction, otherwise a server-issued
        // error mid-stream leaves the cached session pointing at a
        // partially-committed turn.
        let provider = RsclawProvider::new("http://x", None);
        provider.store(
            "k",
            SessionEntry {
                session_id: "rs_w7_abc".into(),
                prefix_id: "rsclaw/test".into(),
                last_seen_msgs_len: 5,
            },
        );
        let inner: LlmStream = Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::Error("model_overloaded".into())),
        ]));
        let wrapped = invalidate_on_error(inner, Arc::clone(&provider.sessions), "k".into());
        let _: Vec<_> = wrapped.collect().await;
        assert!(provider.lock_sessions().get("k").is_none());
    }

    #[tokio::test]
    async fn invalidate_on_error_keeps_session_on_clean_stream() {
        // Stream with no errors → session stays cached. Otherwise we'd
        // pay an unnecessary replay round-trip on every successful
        // turn, defeating kvCacheMode=2.
        let provider = RsclawProvider::new("http://x", None);
        provider.store(
            "k",
            SessionEntry {
                session_id: "rs_w7_abc".into(),
                prefix_id: "rsclaw/test".into(),
                last_seen_msgs_len: 5,
            },
        );
        let inner: LlmStream = Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::TextDelta("hello".into())),
            Ok(StreamEvent::Done { usage: None }),
        ]));
        let wrapped = invalidate_on_error(inner, Arc::clone(&provider.sessions), "k".into());
        let _: Vec<_> = wrapped.collect().await;
        assert!(provider.lock_sessions().get("k").is_some());
    }

    #[test]
    fn turn_delta_collects_parallel_tool_results() {
        // Assistant called 3 tools in parallel → 3 trailing Tool messages.
        let tool_msg = |id: &str, body: &str| Message {
            role: Role::Tool,
            content: MessageContent::Parts(vec![ContentPart::ToolResult {
                tool_use_id: id.into(),
                content: body.into(),
                is_error: None,
            }]),
        };
        let req = req_with(
            vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("do three things".into()),
                },
                tool_msg("toolu_a", "result a"),
                tool_msg("toolu_b", "result b"),
                tool_msg("toolu_c", "result c"),
            ],
            2,
            Some("k"),
        );
        let delta = TurnDelta::from_request(&req).unwrap();
        let body = serde_json::to_value(&delta).unwrap();
        let arr = body["tool_results"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["tool_use_id"], "toolu_a");
        assert_eq!(arr[1]["tool_use_id"], "toolu_b");
        assert_eq!(arr[2]["tool_use_id"], "toolu_c");
    }

    #[test]
    fn turn_delta_does_not_cross_user_boundary() {
        // A non-Tool message between User and the trailing Tool must
        // stop the back-walk — the earlier Tool belongs to a prior turn.
        let req = req_with(
            vec![
                Message {
                    role: Role::Tool,
                    content: MessageContent::Parts(vec![ContentPart::ToolResult {
                        tool_use_id: "toolu_old".into(),
                        content: "stale".into(),
                        is_error: None,
                    }]),
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("ack".into()),
                },
                Message {
                    role: Role::Tool,
                    content: MessageContent::Parts(vec![ContentPart::ToolResult {
                        tool_use_id: "toolu_new".into(),
                        content: "fresh".into(),
                        is_error: None,
                    }]),
                },
            ],
            2,
            Some("k"),
        );
        let delta = TurnDelta::from_request(&req).unwrap();
        let body = serde_json::to_value(&delta).unwrap();
        let arr = body["tool_results"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["tool_use_id"], "toolu_new");
    }

    #[test]
    fn split_system_messages_lifts_system_to_suffix() {
        // Mid-conversation Role::System blocks (plugins, skills,
        // /ctx) must NOT appear in /sessions/replay history — the
        // protocol rejects role:"system" with 400 invalid_history.
        let m = |role: Role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let msgs = vec![
            m(Role::System, "PLUGINS"),
            m(Role::System, "SKILLS"),
            m(Role::User, "hi"),
            m(Role::Assistant, "yo"),
            m(Role::System, "## New Skill Installed\nfoo"),
            m(Role::User, "again"),
        ];
        let (filtered, suffix) = split_system_messages(&msgs);
        assert_eq!(filtered.len(), 3);
        for m in &filtered {
            assert!(!matches!(m.role, Role::System));
        }
        assert_eq!(suffix, "PLUGINS\n\nSKILLS\n\n## New Skill Installed\nfoo");
    }

    #[test]
    fn split_system_messages_handles_text_parts() {
        let msgs = vec![Message {
            role: Role::System,
            content: MessageContent::Parts(vec![
                ContentPart::Text { text: "hello ".into() },
                ContentPart::Text { text: "world".into() },
            ]),
        }];
        let (filtered, suffix) = split_system_messages(&msgs);
        assert!(filtered.is_empty());
        assert_eq!(suffix, "hello world");
    }

    #[test]
    fn split_system_messages_empty_when_no_system() {
        let msgs = vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
        }];
        let (filtered, suffix) = split_system_messages(&msgs);
        assert_eq!(filtered.len(), 1);
        assert!(suffix.is_empty());
    }

    #[test]
    fn split_system_messages_drops_empty_text_system() {
        // An empty System(Text("")) used to leak into sys_parts and
        // produce a stray "\n\n" prefix once joined. Verify it now
        // drops cleanly, matching the Parts path's behavior.
        let msgs = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
            },
            Message {
                role: Role::System,
                content: MessageContent::Text(String::new()),
            },
            Message {
                role: Role::System,
                content: MessageContent::Text("real ctx".into()),
            },
        ];
        let (filtered, suffix) = split_system_messages(&msgs);
        assert_eq!(filtered.len(), 1);
        assert_eq!(
            suffix, "real ctx",
            "leading empty System must not produce a blank-line prefix; got {suffix:?}"
        );
    }

    #[test]
    fn split_system_messages_drops_parts_with_only_empty_text() {
        // Same symmetry check on the Parts path — ensure there's no
        // regression from the unification.
        let msgs = vec![Message {
            role: Role::System,
            content: MessageContent::Parts(vec![
                ContentPart::Text { text: String::new() },
                ContentPart::Image { url: "https://x/i".into() },
            ]),
        }];
        let (_filtered, suffix) = split_system_messages(&msgs);
        assert!(
            suffix.is_empty(),
            "Parts whose only Text was empty must not leak; got {suffix:?}"
        );
    }

    #[test]
    fn normalize_trailing_system_folds_into_user_text() {
        // Runtime appended a dynamic-ctx Role::System after the User
        // delta — fold it into the user_message body so from_request
        // sees a User-trailing list.
        let m = |role: Role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let mut msgs = vec![
            m(Role::User, "fix the bug"),
            m(Role::System, "## Dynamic /ctx\nworking on handler.py"),
        ];
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].role, Role::User));
        let MessageContent::Text(t) = &msgs[0].content else {
            panic!("expected Text content")
        };
        assert_eq!(t, "fix the bug\n\n## Dynamic /ctx\nworking on handler.py");
    }

    #[test]
    fn normalize_trailing_system_concatenates_multiple_in_order() {
        // Two runtime-appended System blocks (e.g. new_skills_tail +
        // dynamic_ctx) concatenated in original order, joined by \n\n.
        let m = |role: Role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let mut msgs = vec![
            m(Role::User, "go"),
            m(Role::System, "FIRST"),
            m(Role::System, "SECOND"),
        ];
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 1);
        let MessageContent::Text(t) = &msgs[0].content else {
            panic!("expected Text content")
        };
        assert_eq!(t, "go\n\nFIRST\n\nSECOND");
    }

    #[test]
    fn normalize_trailing_system_noop_without_trailing_system() {
        // No System anywhere — list is unchanged.
        let m = |role: Role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let original = vec![m(Role::User, "hi"), m(Role::Assistant, "yo")];
        let mut msgs = original.clone();
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 2);
        let MessageContent::Text(last) = &msgs[1].content else {
            panic!()
        };
        assert_eq!(last, "yo");
    }

    #[test]
    fn normalize_trailing_system_folds_into_user_parts() {
        // User content already in Parts form (text + image) — fold
        // System text by appending a new Text part rather than mutating
        // an existing part.
        let mut msgs = vec![
            Message {
                role: Role::User,
                content: MessageContent::Parts(vec![
                    ContentPart::Text { text: "look at this".into() },
                    ContentPart::Image { url: "https://x/y.png".into() },
                ]),
            },
            Message {
                role: Role::System,
                content: MessageContent::Text("CTX".into()),
            },
        ];
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 1);
        let MessageContent::Parts(parts) = &msgs[0].content else {
            panic!("expected Parts content")
        };
        assert_eq!(parts.len(), 3);
        match &parts[2] {
            ContentPart::Text { text } => assert_eq!(text, "CTX"),
            _ => panic!("expected appended Text part"),
        }
    }

    #[test]
    fn normalize_trailing_system_drops_when_preceded_by_tool() {
        // Defensive — runtime doesn't currently inject System after
        // Tool, but if it ever did, we can't fold into a tool_results
        // delta. Drop the System text, leave the Tool tail intact so
        // from_request can build a Tools delta.
        let mut msgs = vec![
            Message {
                role: Role::Tool,
                content: MessageContent::Parts(vec![ContentPart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "result".into(),
                    is_error: None,
                }]),
            },
            Message {
                role: Role::System,
                content: MessageContent::Text("dynamic ctx".into()),
            },
        ];
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].role, Role::Tool));
    }

    #[test]
    fn normalize_trailing_system_skips_empty_system_blocks() {
        // An empty Role::System (text=="") shouldn't add stray "\n\n"
        // separators. Drop empty ones; fold non-empty ones.
        let m = |role: Role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let mut msgs = vec![
            m(Role::User, "hi"),
            m(Role::System, ""),
            m(Role::System, "non-empty"),
        ];
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 1);
        let MessageContent::Text(t) = &msgs[0].content else {
            panic!()
        };
        assert_eq!(t, "hi\n\nnon-empty");
    }

    #[test]
    fn turn_options_temperature_clamps_to_two_decimals() {
        // Default serde f32 serialization leaks IEEE 754 noise:
        // `0.6_f32` lifts to f64 `0.6000000238418579`, which makes the
        // request body ugly, breaks request hashing for caching layers,
        // and confuses anyone tailing logs. ser_opt_f32 routes through
        // super::json_f32 which rounds to 2 decimals.
        let opts = TurnOptions {
            max_tokens: None,
            temperature: Some(0.6),
            top_p: Some(0.95),
            enable_thinking: None,
            stop: None,
            idle_ttl_secs: None,
        };
        let body = serde_json::to_value(&opts).unwrap();
        // serde_json compares numbers by value not by string repr, so
        // an Eq against json!(0.6) would succeed even with the buggy
        // path. Compare the serialized text instead.
        let s = serde_json::to_string(&opts).unwrap();
        assert!(
            s.contains("\"temperature\":0.6"),
            "expected temperature:0.6, got {s}"
        );
        assert!(!s.contains("0.6000000238418579"), "leaked f32→f64 noise: {s}");
        assert!(
            s.contains("\"top_p\":0.95"),
            "expected top_p:0.95, got {s}"
        );
        // sanity — body is still well-formed JSON.
        assert!(body.is_object());
    }

    #[test]
    fn turn_options_temperature_none_omits_field() {
        let opts = TurnOptions {
            max_tokens: None,
            temperature: None,
            top_p: None,
            enable_thinking: None,
            stop: None,
            idle_ttl_secs: None,
        };
        let body = serde_json::to_value(&opts).unwrap();
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
    }

    #[test]
    fn create_session_resp_parses_replay_shape_without_prefix_id() {
        // Protocol §2.2 replay response carries session_id but NOT
        // prefix_id. Without #[serde(default)] this fails with
        // "missing field prefix_id" and breaks every replay path.
        let body = r#"{
            "session_id": "rs_w7_8a3c1f2b",
            "n_prefix_tokens": 27981,
            "n_user_tokens": 612,
            "n_history_tokens": 8420,
            "n_tokens": 37013,
            "instance_id": "llama-worker-7",
            "replay_ms": 2340
        }"#;
        let resp: CreateSessionResp = serde_json::from_str(body).expect("replay shape parses");
        assert_eq!(resp.session_id, "rs_w7_8a3c1f2b");
        assert!(resp.prefix_id.is_none());
    }

    #[test]
    fn create_session_resp_parses_create_shape_with_prefix_id() {
        // Protocol §2.1.6 (post-rename) create response carries
        // `prefix_id`. New servers send this name natively.
        let body = r#"{
            "session_id": "rs_w7_8a3c1f2b",
            "prefix_id": "rsclaw/2026.5.18"
        }"#;
        let resp: CreateSessionResp = serde_json::from_str(body).expect("create shape parses");
        assert_eq!(resp.prefix_id.as_deref(), Some("rsclaw/2026.5.18"));
    }

    #[test]
    fn create_session_resp_ignores_unknown_legacy_rsclaw_version() {
        // Pre-rename `rsclaw_version` is being dropped server-side
        // entirely. While some builds still emit it alongside
        // `prefix_id` mid-roll, our struct treats it as an unknown
        // key and serde ignores it silently. Production e2e against
        // `:8443` sends exactly this shape:
        //   {"prefix_id":"dynamic/...","prefix_source":"dynamic_miss",
        //    "rsclaw_version":""}
        // Without this regression test the prior `serde(alias)`
        // approach would resurface and trip `duplicate field` errors
        // again.
        let body = r#"{
            "session_id":"rs_w7_8cebc736",
            "prefix_id":"dynamic/9e8598684ad34ff0a615899fefb811de",
            "prefix_source":"dynamic_miss",
            "rsclaw_version":""
        }"#;
        let resp: CreateSessionResp = serde_json::from_str(body)
            .expect("mixed post-rename + legacy fields must parse");
        assert_eq!(resp.session_id, "rs_w7_8cebc736");
        assert_eq!(
            resp.prefix_id.as_deref(),
            Some("dynamic/9e8598684ad34ff0a615899fefb811de"),
        );
    }

    #[test]
    fn create_session_resp_parses_explicit_null_prefix_id() {
        // `prefix_id: String` with `#[serde(default)]` would FAIL
        // parsing on explicit JSON null with "invalid type: null,
        // expected a string", tanking the whole `/sessions` (or
        // `/sessions/replay`) response and surfacing as an opaque
        // "parse response" error to the caller. Upstream nodes
        // occasionally emit null while the version registry is
        // mid-roll. Option<String> accepts null → None and keeps the
        // rest of the response usable.
        let body = r#"{"session_id":"rs_a_b","prefix_id":null}"#;
        let resp: CreateSessionResp =
            serde_json::from_str(body).expect("null prefix_id must parse");
        assert_eq!(resp.session_id, "rs_a_b");
        assert!(resp.prefix_id.is_none());
    }

    #[test]
    fn create_session_resp_parses_missing_prefix_id() {
        // The replay response per §2.2 omits prefix_id entirely.
        // Behaviour must match the explicit-null case: parse cleanly,
        // surface None.
        let body = r#"{"session_id":"rs_a_b"}"#;
        let resp: CreateSessionResp = serde_json::from_str(body).expect("missing field must parse");
        assert!(resp.prefix_id.is_none());
    }

    #[test]
    fn create_session_resp_parses_populated_prefix_id() {
        // Round-trip the happy path so the Option<String> change
        // doesn't accidentally start coercing real values to None.
        let body = r#"{"session_id":"rs_a_b","prefix_id":"rsclaw/2026.5.18"}"#;
        let resp: CreateSessionResp = serde_json::from_str(body).expect("string field must parse");
        assert_eq!(resp.prefix_id.as_deref(), Some("rsclaw/2026.5.18"));
    }

    // -- 8-rule dispatch precedence (R1 C3) -----------------------------
    //
    // Table-driven coverage so a future drift between the comment-table
    // in `dispatch_decision` and the runtime decision is caught.

    fn dispatch_req(
        model: &str,
        endpoint: AgentEndpoint,
        session_key: Option<&str>,
        kv_cache_mode: u8,
    ) -> LlmRequest {
        LlmRequest {
            model: model.into(),
            endpoint,
            kv_cache_mode,
            session_key: session_key.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn dispatch_rule_1_flash_model_routes_fastshot() {
        // Rule 1: rsclaw-flash-* wins regardless of endpoint hint.
        let route = dispatch_decision(&dispatch_req(
            "rsclaw/rsclaw-flash-v1",
            AgentEndpoint::Primary,
            None,
            0,
        ))
        .unwrap();
        assert_eq!(route, DispatchRoute::OneShot("/fastshot"));

        // Even with explicit Vision endpoint hint, model name still wins.
        let route = dispatch_decision(&dispatch_req(
            "rsclaw/rsclaw-flash-v1",
            AgentEndpoint::Vision,
            None,
            0,
        ))
        .unwrap();
        assert_eq!(route, DispatchRoute::OneShot("/fastshot"));
    }

    #[test]
    fn dispatch_rule_2_vision_model_routes_vision() {
        let route = dispatch_decision(&dispatch_req(
            "rsclaw/rsclaw-vision-v1",
            AgentEndpoint::Primary,
            None,
            0,
        ))
        .unwrap();
        assert_eq!(route, DispatchRoute::OneShot("/vision"));
    }

    #[test]
    fn dispatch_rule_3_agent_model_no_session_routes_oneshot() {
        // Per R2 review: stateless agent call must NOT bail; routes to
        // /oneshot per server hint "use /v1/agent/oneshot for agent model".
        let route = dispatch_decision(&dispatch_req(
            "rsclaw/rsclaw-agent-v1",
            AgentEndpoint::Primary,
            None,
            0,
        ))
        .unwrap();
        assert_eq!(route, DispatchRoute::OneShot("/oneshot"));
    }

    #[test]
    fn dispatch_rule_4_agent_model_with_session_routes_sessions() {
        let route = dispatch_decision(&dispatch_req(
            "rsclaw/rsclaw-agent-v1",
            AgentEndpoint::Primary,
            Some("sess-x"),
            2,
        ))
        .unwrap();
        assert_eq!(route, DispatchRoute::Sessions);
    }

    #[test]
    fn dispatch_rule_5_non_canonical_flash_endpoint_routes_fastshot() {
        // Rule 5: non-canonical model + endpoint=Flash hint → /fastshot.
        let route = dispatch_decision(&dispatch_req(
            "anthropic/claude-3-5-haiku",
            AgentEndpoint::Flash,
            None,
            0,
        ))
        .unwrap();
        assert_eq!(route, DispatchRoute::OneShot("/fastshot"));
    }

    #[test]
    fn dispatch_rule_6_non_canonical_vision_endpoint_routes_vision() {
        let route = dispatch_decision(&dispatch_req(
            "anthropic/claude-3-5-sonnet",
            AgentEndpoint::Vision,
            None,
            0,
        ))
        .unwrap();
        assert_eq!(route, DispatchRoute::OneShot("/vision"));
    }

    #[test]
    fn dispatch_rule_7_primary_with_session_routes_sessions() {
        let route = dispatch_decision(&dispatch_req(
            "anthropic/claude-3-5-sonnet",
            AgentEndpoint::Primary,
            Some("sess-y"),
            2,
        ))
        .unwrap();
        assert_eq!(route, DispatchRoute::Sessions);
    }

    #[test]
    fn dispatch_rule_8_primary_stateless_routes_oneshot() {
        let route = dispatch_decision(&dispatch_req(
            "anthropic/claude-3-5-sonnet",
            AgentEndpoint::Primary,
            None,
            0,
        ))
        .unwrap();
        assert_eq!(route, DispatchRoute::OneShot("/oneshot"));
    }

    #[test]
    fn dispatch_bail_kv2_without_session_key() {
        // Safety net (R1 C2): kv_cache_mode=2 + no session_key bails
        // BEFORE routing, so caller can't silently downgrade to /oneshot
        // and lose kvCache continuity.
        let err = dispatch_decision(&dispatch_req(
            "anthropic/claude-3-5-sonnet",
            AgentEndpoint::Primary,
            None,
            2,
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("session_key"), "got: {err}");
        assert!(err.contains("kv_cache_mode=2"), "got: {err}");
    }

    #[test]
    fn dispatch_bail_session_without_kv2() {
        // Sessions path requires kv_cache_mode=2.
        let err = dispatch_decision(&dispatch_req(
            "anthropic/claude-3-5-sonnet",
            AgentEndpoint::Primary,
            Some("sess-z"),
            1,
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("kv_cache_mode=2"), "got: {err}");
    }

    #[test]
    fn dispatch_canonical_model_overrides_endpoint_hint() {
        // Rule 1 wins over endpoint=Vision when model is flash family.
        // Important: covers the case where a misconfigured caller sets
        // a Flash endpoint hint but resolved a non-flash agent model.
        let route = dispatch_decision(&dispatch_req(
            "rsclaw/rsclaw-flash-v1",
            AgentEndpoint::Primary,
            Some("sess-q"),
            2,
        ))
        .unwrap();
        // Rule 1 fires regardless of session_key / kv_cache_mode,
        // because the server-side /fastshot whitelist accepts only
        // rsclaw-flash-*. (Server may 400 on the session_key field but
        // routing is correct at the client.)
        assert_eq!(route, DispatchRoute::OneShot("/fastshot"));
    }

    #[test]
    fn dispatch_rule_3_overrides_rule_5_for_agent_model() {
        // Rule 3 (agent + no session) fires before rule 5 (endpoint=Flash).
        // Caller passing Flash hint on an agent-* model → /oneshot, NOT
        // /fastshot (server would 400 the agent model on /fastshot).
        let route = dispatch_decision(&dispatch_req(
            "rsclaw/rsclaw-agent-v1",
            AgentEndpoint::Flash,
            None,
            0,
        ))
        .unwrap();
        assert_eq!(route, DispatchRoute::OneShot("/oneshot"));
    }

    #[test]
    fn rejects_non_kv2_mode() {
        let provider = RsclawProvider::new("http://x", None);
        let req = req_with(vec![], 1, Some("k"));
        let err = match futures::executor::block_on(provider.stream(req)) {
            Ok(_) => panic!("expected error for kv_cache_mode=1"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("kv_cache_mode=2"));
    }

    // Renamed + re-scoped: the previous version expected an error when
    // session_key was `None` with kv_cache_mode=2, but the dispatch
    // refactor (commit cc6314a) now routes session_key=None to /oneshot
    // regardless of kv_cache_mode. The remaining session-mode contract
    // worth pinning is the inverse: session_key=Some + kv_cache_mode!=2
    // must error rather than silently mis-route. (Also: tokio::test
    // instead of futures::executor::block_on — provider.stream calls
    // into reqwest, which needs a tokio reactor on the current thread.)
    #[tokio::test]
    async fn rejects_session_mode_without_kv_cache_mode_2() {
        let provider = RsclawProvider::new("http://x", None);
        let req = req_with(vec![], 0, Some("session-xyz"));
        let err = match provider.stream(req).await {
            Ok(_) => panic!("expected error for session_key + kv_cache_mode!=2"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("kv_cache_mode=2"),
            "unexpected error text: {err}"
        );
    }

    // ----- compact splice wire shape (§2.4) -------------------------------

    #[test]
    fn compact_splice_req_serialises_post_2_4_shape() {
        // Pin the wire shape so rsclaw-server and gateway can't drift
        // independently. expected_msgs_count is optional; when None it
        // MUST be omitted from the body (not emitted as `"expected_msgs_count": null`)
        // so a server that hasn't shipped the field yet doesn't 400.
        let body = CompactSpliceReq {
            keep_head_messages: 2,
            summary: "<sum>",
            keep_tail_messages: 10,
            expected_msgs_count: Some(80),
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["keep_head_messages"], 2);
        assert_eq!(v["summary"], "<sum>");
        assert_eq!(v["keep_tail_messages"], 10);
        assert_eq!(v["expected_msgs_count"], 80);

        let body_no_expect = CompactSpliceReq {
            keep_head_messages: 2,
            summary: "<sum>",
            keep_tail_messages: 10,
            expected_msgs_count: None,
        };
        let v_no_expect = serde_json::to_value(&body_no_expect).unwrap();
        assert!(
            v_no_expect.get("expected_msgs_count").is_none(),
            "None must be omitted from the wire body, not emitted as null"
        );
    }

    #[test]
    fn compact_splice_resp_parses_happy_shape() {
        let body = r#"{"session_id":"rs_w7_abc","msgs_count":13,"tokens_count":8421}"#;
        let resp: CompactSpliceResp =
            serde_json::from_str(body).expect("happy compact response must parse");
        assert_eq!(resp.session_id, "rs_w7_abc");
        assert_eq!(resp.msgs_count, 13);
        assert_eq!(resp.tokens_count, 8421);
    }

    #[test]
    fn compact_splice_trait_default_returns_err_for_non_rsclaw() {
        // Trait-level default impl: non-rsclaw providers should bail
        // with a "not supported" error so callers can fall back cleanly.
        // Sanity-check on a placeholder provider via the public trait.
        use crate::provider::LlmProvider;
        struct StubProvider;
        impl LlmProvider for StubProvider {
            fn name(&self) -> &str { "stub" }
            fn stream(
                &self,
                _req: crate::provider::LlmRequest,
            ) -> futures::future::BoxFuture<'_, anyhow::Result<crate::provider::LlmStream>> {
                Box::pin(async { anyhow::bail!("stub provider has no streaming") })
            }
        }
        let p = StubProvider;
        let err = futures::executor::block_on(
            p.compact_splice("k", 2, "x", 10, None)
        ).expect_err("default impl must Err");
        let msg = err.to_string();
        assert!(
            msg.contains("not supported") && msg.contains("stub"),
            "default impl Err should name the provider: {msg}"
        );
    }

    #[tokio::test]
    async fn compact_splice_errs_when_no_cached_session() {
        // Splice short-circuits BEFORE any HTTP call when the cached
        // SessionEntry for `session_key` is missing — no point splicing
        // a session we don't think is open. Caller (compact_inner)
        // observes the Err and falls back to replay path.
        use crate::provider::LlmProvider;
        let provider = RsclawProvider::new("http://nonexistent-host.invalid", None);
        let err = provider
            .compact_splice("missing-key", 2, "summary", 10, None)
            .await
            .expect_err("should Err when no cached SessionEntry exists");
        let msg = err.to_string();
        assert!(
            msg.contains("no cached session"),
            "Err message should mention missing cached session, got: {msg}"
        );
    }

    #[tokio::test]
    async fn compact_splice_updates_last_seen_msgs_len_on_success() {
        // Pin the critical post-splice state mutation: on HTTP success
        // the cached SessionEntry.last_seen_msgs_len MUST be updated to
        // the gateway-local computation (head + 1 + tail). Without this
        // update, the next turn's lookup_and_bump would (incorrectly)
        // see msgs.len() < last_seen and force an unnecessary replay.
        use crate::provider::LlmProvider;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        let session_id = "rs_w7_abc";

        Mock::given(method("POST"))
            .and(path(format!("/sessions/{}/compact", session_id)))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "session_id": session_id,
                    "msgs_count": 13,
                    "tokens_count": 8421,
                })),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let provider = RsclawProvider::new(mock_server.uri(), None);

        // Pre-populate the cached SessionEntry so the splice has
        // something to operate against. last_seen_msgs_len starts at 50
        // (a typical pre-compact value) so we can verify it's updated
        // to 13 (head=2 + summary=1 + tail=10) after success.
        {
            let mut map = provider.lock_sessions();
            map.insert(
                "test-key".to_owned(),
                SessionEntry {
                    session_id: session_id.to_owned(),
                    prefix_id: RSCLAW_DEFAULT_PREFIX_ID.to_owned(),
                    last_seen_msgs_len: 50,
                },
            );
        }

        let result = provider
            .compact_splice("test-key", 2, "<summary>", 10, Some(50))
            .await
            .expect("happy-path splice should succeed");
        assert_eq!(result, 13, "trait method returns server's msgs_count");

        let map = provider.lock_sessions();
        let entry = map
            .get("test-key")
            .expect("SessionEntry must still exist after splice — id is preserved");
        assert_eq!(
            entry.last_seen_msgs_len, 13,
            "last_seen_msgs_len must be updated to head(2) + summary(1) + tail(10)"
        );
        assert_eq!(
            entry.session_id, session_id,
            "session_id MUST be unchanged across splice (§2.4 invariant)"
        );
        assert_eq!(
            entry.prefix_id, RSCLAW_DEFAULT_PREFIX_ID,
            "prefix_id must be unchanged"
        );
    }
}
