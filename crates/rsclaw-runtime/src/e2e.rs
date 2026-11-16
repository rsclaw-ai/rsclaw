//! End-to-end encrypted sessions between an app and this gateway.
//!
//! A relay between the app and the gateway (or anyone on the LAN) sees only
//! ciphertext and lengths: not the path, not the credential, not the body.
//!
//! Protocol `rsclaw-e2e/1`:
//!
//! - **Gateway identity.** A long-term X25519 key in `var/data/e2e_key.json`.
//!   The app learns its public half out of band (the pairing QR code,
//!   `e2e_pub`), so it knows which gateway it trusts before it connects.
//! - **Handshake.** `Noise_IK_25519_ChaChaPoly_BLAKE2s`, prologue
//!   `rsclaw-e2e/1`, app = initiator with its own long-term device key.
//!   `POST /api/v1/e2e/handshake` `{"v":1,"m":"<base64 msg1>"}` →
//!   `{"v":1,"m":"<base64 msg2>"}`. The msg1 payload is JSON
//!   `{"token":"<gateway or scoped token>","ts":<unix secs>}`; the msg2
//!   payload is JSON `{"sid","expires_at","principal"}`. A handshake more
//!   than five minutes off the gateway clock, or seen before, is refused. A
//!   scoped token is bound to the first device key that presents it.
//! - **Transport.** The Noise split gives `k1` (app → gateway) and `k2`
//!   (gateway → app). Each message is ChaCha20-Poly1305 with nonce
//!   `0x00000000 || u64 LE n` and associated data = the sid. Each side
//!   numbers its own messages from 0; the gateway refuses a request number
//!   it has seen or one more than 1024 behind the highest it has seen.
//! - **Calls.** `POST /api/v1/e2e/call?sid=<sid>` `{"n":<n>,"c":"<base64>"}`.
//!   The plaintext is JSON `{"method","path","headers"?,"body"?}` (path with
//!   query, `/api/v1/...`; body base64). The reply is `{"n","c"}` whose
//!   plaintext is `{"status","headers","body"}`.
//! - **Streams.** `GET /api/v1/e2e/events?sid=&n=&c=<base64url request>`
//!   for a GET whose response streams (SSE). Every outer SSE event's data is
//!   `{"n","c"}`: the first (`event: head`) holds `{"status","headers"}`, and
//!   each later one (`event: data`) the next raw bytes of the inner body.
//! - **Sessions** last 24 hours. Every call re-checks the credential, so
//!   revoking a scoped token or rotating the gateway token ends its sessions.
//!
//! Inner requests run through the full router, auth middleware included:
//! the session's principal stands in for the bearer token.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, Query, State},
    http::{HeaderName, HeaderValue, Method, Request, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tower::ServiceExt as _;
use tracing::{info, warn};

use crate::{
    access::{AccessTokens, Principal, TOKEN_PREFIX, now_secs, random_bytes},
    server::AppState,
};

/// What the access-token and session handlers need: a slice of
/// [`AppState`], so they can also run in a small router in tests.
#[derive(Clone)]
pub struct Gate {
    pub e2e: Arc<E2e>,
    pub tokens: Arc<AccessTokens>,
    pub gateway: Arc<tokio::sync::RwLock<rsclaw_config::runtime::GatewayRuntime>>,
}

impl axum::extract::FromRef<AppState> for Gate {
    fn from_ref(state: &AppState) -> Self {
        Self {
            e2e: Arc::clone(&state.e2e),
            tokens: Arc::clone(&state.access_tokens),
            gateway: Arc::clone(&state.live.gateway),
        }
    }
}

/// Admit a request that arrived inside a session: check the session's
/// principal may reach the path and hand it to the route. `None` when the
/// request did not come from a session. The auth middleware calls this
/// before looking at any bearer token.
pub(crate) fn admit(request: &mut axum::extract::Request) -> Option<Result<(), Response>> {
    let inner = request.extensions().get::<E2eRequest>().cloned()?;
    if !crate::access::may_reach(&inner.principal, request.uri().path()) {
        return Some(Err(error(StatusCode::FORBIDDEN, "out_of_scope")));
    }
    request.extensions_mut().insert(inner.principal);
    Some(Ok(()))
}

pub const PROTOCOL: &str = "rsclaw-e2e/1";
const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
const HANDSHAKE_SKEW_SECS: u64 = 300;
const SESSION_TTL_SECS: u64 = 24 * 60 * 60;
const REPLAY_WINDOW: u64 = 1024;
const MAX_SESSIONS_PER_CREDENTIAL: usize = 16;
const MAX_SESSIONS: usize = 1024;
const MAX_HANDSHAKE_BYTES: usize = 4096;
/// Largest inner request or response body a call carries.
pub const MAX_INNER_BODY: usize = 32 * 1024 * 1024;
/// Largest outer call body: the inner body in base64 plus framing.
pub(crate) const MAX_ENVELOPE_BODY: usize = MAX_INNER_BODY / 3 * 4 + 64 * 1024;
const INNER_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_RECHECK: Duration = Duration::from_secs(60);
/// Inner request headers passed through; everything else is dropped.
const INNER_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "content-type",
    "if-none-match",
    "last-event-id",
];

/// Marks a request that arrived inside a session. Only this module creates
/// it; nothing from the network can.
#[derive(Debug, Clone)]
pub(crate) struct E2eRequest {
    pub principal: Principal,
}

#[derive(Serialize, Deserialize)]
struct KeyFile {
    v: u8,
    private: String,
    public: String,
}

struct Identity {
    private: Vec<u8>,
    public: Vec<u8>,
}

enum Credential {
    /// The gateway token, by its generation (`None` for an open gateway).
    Admin {
        generation: Option<String>,
    },
    Scoped {
        token_id: String,
    },
}

impl Credential {
    fn key(&self) -> &str {
        match self {
            Self::Admin { .. } => "admin",
            Self::Scoped { token_id } => token_id,
        }
    }
}

struct Session {
    sid: String,
    credential: Credential,
    recv: ChaCha20Poly1305,
    send: ChaCha20Poly1305,
    window: std::sync::Mutex<ReplayWindow>,
    send_n: AtomicU64,
    created_at: u64,
    expires_at: u64,
}

impl Session {
    fn open(&self, n: u64, ciphertext: &[u8]) -> Result<Vec<u8>, Response> {
        let plain = decrypt(&self.recv, n, self.sid.as_bytes(), ciphertext)
            .ok_or_else(|| error(StatusCode::BAD_REQUEST, "bad_envelope"))?;
        let fresh = self
            .window
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .accept(n);
        if !fresh {
            return Err(error(StatusCode::BAD_REQUEST, "replayed"));
        }
        Ok(plain)
    }

    fn seal(&self, plain: &[u8]) -> serde_json::Value {
        let n = self.send_n.fetch_add(1, Ordering::Relaxed);
        let c = encrypt(&self.send, n, self.sid.as_bytes(), plain);
        serde_json::json!({"n": n, "c": STANDARD.encode(c)})
    }
}

/// Request numbers seen recently, so none is accepted twice.
#[derive(Default)]
struct ReplayWindow {
    top: Option<u64>,
    seen: BTreeSet<u64>,
}

impl ReplayWindow {
    fn accept(&mut self, n: u64) -> bool {
        if let Some(top) = self.top
            && (n.saturating_add(REPLAY_WINDOW) <= top || self.seen.contains(&n))
        {
            return false;
        }
        self.seen.insert(n);
        let top = self.top.map_or(n, |t| t.max(n));
        self.top = Some(top);
        self.seen = self.seen.split_off(&top.saturating_sub(REPLAY_WINDOW));
        true
    }
}

fn nonce(n: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[4..].copy_from_slice(&n.to_le_bytes());
    out
}

fn encrypt(key: &ChaCha20Poly1305, n: u64, ad: &[u8], plain: &[u8]) -> Vec<u8> {
    key.encrypt(
        &nonce(n).into(),
        Payload {
            msg: plain,
            aad: ad,
        },
    )
    .expect("chacha20poly1305 encrypt is infallible for in-memory buffers")
}

fn decrypt(key: &ChaCha20Poly1305, n: u64, ad: &[u8], ciphertext: &[u8]) -> Option<Vec<u8>> {
    key.decrypt(
        &nonce(n).into(),
        Payload {
            msg: ciphertext,
            aad: ad,
        },
    )
    .ok()
}

fn noise_params() -> snow::params::NoiseParams {
    NOISE_PARAMS.parse().expect("valid noise params")
}

/// The gateway's identity key and its live sessions.
pub struct E2e {
    identity: Option<Identity>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Digests of accepted handshakes and when they may be forgotten.
    seen: Mutex<HashMap<[u8; 32], u64>>,
    router: std::sync::RwLock<Option<Router>>,
}

impl E2e {
    /// Load the identity key from `path`, creating it when the file is
    /// missing. A file that exists but cannot be used leaves sessions off
    /// (handshakes answer 503) rather than replacing the key, which would
    /// silently unpair every device.
    pub fn load(path: PathBuf) -> Self {
        let identity = match std::fs::read_to_string(&path) {
            Ok(raw) => match parse_key_file(&raw) {
                Some(id) => Some(id),
                None => {
                    warn!(path = %path.display(), "e2e key file unusable; encrypted sessions are off");
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match create_key_file(&path) {
                Ok(id) => {
                    info!(path = %path.display(), "created e2e identity key");
                    Some(id)
                }
                Err(e) => {
                    warn!(error = %e, path = %path.display(), "cannot create e2e key; encrypted sessions are off");
                    None
                }
            },
            Err(e) => {
                warn!(error = %e, path = %path.display(), "cannot read e2e key; encrypted sessions are off");
                None
            }
        };
        Self::with_identity(identity)
    }

    fn with_identity(identity: Option<Identity>) -> Self {
        Self {
            identity,
            sessions: Mutex::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
            router: std::sync::RwLock::new(None),
        }
    }

    /// A gateway identity that lives only in memory, for tests.
    pub fn ephemeral() -> Self {
        let kp = snow::Builder::new(noise_params())
            .generate_keypair()
            .expect("generate keypair");
        Self::with_identity(Some(Identity {
            private: kp.private,
            public: kp.public,
        }))
    }

    /// The identity's public key in base64, for the pairing QR code.
    pub fn public_key_b64(&self) -> Option<String> {
        self.identity.as_ref().map(|id| STANDARD.encode(&id.public))
    }

    /// The router inner requests run through. Set when the router is built.
    pub(crate) fn set_router(&self, router: Router) {
        *self
            .router
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(router);
    }

    fn router(&self) -> Option<Router> {
        self.router
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// True when `sid` names a live session (the rate limiter exempts them).
    pub(crate) async fn has_session(&self, sid: &str) -> bool {
        let now = now_secs();
        self.sessions
            .lock()
            .await
            .get(sid)
            .is_some_and(|s| now < s.expires_at)
    }

    /// End every session opened with scoped token `token_id`.
    pub(crate) async fn drop_sessions_of(&self, token_id: &str) {
        self.sessions.lock().await.retain(
            |_, s| !matches!(&s.credential, Credential::Scoped { token_id: id } if id == token_id),
        );
    }

    /// Record a handshake digest; false if it was seen before.
    async fn first_sight(&self, digest: [u8; 32], now: u64) -> bool {
        let mut seen = self.seen.lock().await;
        seen.retain(|_, until| now < *until);
        if seen.contains_key(&digest) {
            return false;
        }
        seen.insert(digest, now + 2 * HANDSHAKE_SKEW_SECS + 1);
        true
    }

    async fn insert(&self, session: Session) {
        let mut sessions = self.sessions.lock().await;
        let now = now_secs();
        sessions.retain(|_, s| now < s.expires_at);
        let key = session.credential.key().to_owned();
        let mine = sessions
            .values()
            .filter(|s| s.credential.key() == key)
            .count();
        if mine >= MAX_SESSIONS_PER_CREDENTIAL {
            evict_oldest(&mut sessions, |s| s.credential.key() == key);
        }
        if sessions.len() >= MAX_SESSIONS {
            evict_oldest(&mut sessions, |_| true);
        }
        sessions.insert(session.sid.clone(), Arc::new(session));
    }

    /// The live session `sid` and who it acts for, re-checking the
    /// credential; a session whose credential is gone is dropped.
    async fn session(
        &self,
        state: &Gate,
        sid: &str,
    ) -> Result<(Arc<Session>, Principal), Response> {
        let now = now_secs();
        let session = self
            .sessions
            .lock()
            .await
            .get(sid)
            .filter(|s| now < s.expires_at)
            .cloned()
            .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "session_expired"))?;
        match principal_of(state, &session, now).await {
            Some(principal) => Ok((session, principal)),
            None => {
                self.sessions.lock().await.remove(sid);
                Err(error(StatusCode::UNAUTHORIZED, "session_revoked"))
            }
        }
    }
}

fn evict_oldest(sessions: &mut HashMap<String, Arc<Session>>, pick: impl Fn(&Session) -> bool) {
    if let Some(sid) = sessions
        .values()
        .filter(|s| pick(s))
        .min_by_key(|s| s.created_at)
        .map(|s| s.sid.clone())
    {
        sessions.remove(&sid);
    }
}

async fn principal_of(state: &Gate, session: &Session, now: u64) -> Option<Principal> {
    match &session.credential {
        Credential::Admin { generation } => {
            let token = state.gateway.read().await.auth_token.clone();
            let current = token.as_deref().map(crate::ws::handshake::token_generation);
            (current == *generation).then_some(Principal::Admin)
        }
        Credential::Scoped { token_id } => state
            .tokens
            .current(token_id, now)
            .await
            .map(Principal::Scoped),
    }
}

fn parse_key_file(raw: &str) -> Option<Identity> {
    let file: KeyFile = serde_json::from_str(raw).ok()?;
    let private = STANDARD.decode(file.private).ok()?;
    let public = STANDARD.decode(file.public).ok()?;
    (file.v == 1 && private.len() == 32 && public.len() == 32)
        .then_some(Identity { private, public })
}

fn create_key_file(path: &std::path::Path) -> std::io::Result<Identity> {
    let kp = snow::Builder::new(noise_params())
        .generate_keypair()
        .map_err(std::io::Error::other)?;
    let file = KeyFile {
        v: 1,
        private: STANDARD.encode(&kp.private),
        public: STANDARD.encode(&kp.public),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write as _;
        opts.open(&tmp)?
            .write_all(serde_json::to_string_pretty(&file)?.as_bytes())?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(Identity {
        private: kp.private,
        public: kp.public,
    })
}

fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(serde_json::json!({"error": code}))).into_response()
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct HandshakeBody {
    v: u8,
    m: String,
}

#[derive(Deserialize)]
struct Hello {
    token: String,
    ts: u64,
}

/// GET /api/v1/e2e/key — the gateway's public key, for an admin building
/// a pairing code. Behind the ordinary gateway-token auth.
pub(crate) async fn public_key(State(state): State<Gate>) -> Response {
    match state.e2e.public_key_b64() {
        Some(key) => {
            Json(serde_json::json!({"e2e_pub": key, "protocol": PROTOCOL})).into_response()
        }
        None => error(StatusCode::SERVICE_UNAVAILABLE, "e2e_unavailable"),
    }
}

/// POST /api/v1/e2e/handshake
pub(crate) async fn handshake(
    State(state): State<Gate>,
    Json(body): Json<HandshakeBody>,
) -> Response {
    let Some(identity) = &state.e2e.identity else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "e2e_unavailable");
    };
    if body.v != 1 {
        return error(StatusCode::BAD_REQUEST, "unsupported_version");
    }
    let Ok(msg1) = STANDARD.decode(body.m.as_bytes()) else {
        return error(StatusCode::BAD_REQUEST, "bad_handshake");
    };
    if msg1.len() > MAX_HANDSHAKE_BYTES {
        return error(StatusCode::BAD_REQUEST, "bad_handshake");
    }
    let built = snow::Builder::new(noise_params())
        .local_private_key(&identity.private)
        .and_then(|b| b.prologue(PROTOCOL.as_bytes()))
        .and_then(|b| b.build_responder());
    let Ok(mut hs) = built else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "e2e_unavailable");
    };
    let mut payload = vec![0u8; msg1.len()];
    let Ok(len) = hs.read_message(&msg1, &mut payload) else {
        return error(StatusCode::BAD_REQUEST, "bad_handshake");
    };
    let Ok(hello) = serde_json::from_slice::<Hello>(&payload[..len]) else {
        return error(StatusCode::BAD_REQUEST, "bad_handshake");
    };
    let Some(device) = hs.get_remote_static().map(<[u8]>::to_vec) else {
        return error(StatusCode::BAD_REQUEST, "bad_handshake");
    };
    let now = now_secs();
    if now.abs_diff(hello.ts) > HANDSHAKE_SKEW_SECS {
        return error(StatusCode::BAD_REQUEST, "stale_handshake");
    }

    let (credential, principal) = if hello.token.starts_with(TOKEN_PREFIX) {
        match state.tokens.authenticate(&hello.token, &device, now).await {
            Ok(p) => (
                Credential::Scoped {
                    token_id: p.token_id.clone(),
                },
                Principal::Scoped(p),
            ),
            Err(e) => {
                warn!(reason = e.code(), "e2e handshake refused");
                let status = if e == crate::access::AccessError::OtherDevice {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::UNAUTHORIZED
                };
                return error(status, e.code());
            }
        }
    } else {
        let admin = state.gateway.read().await.auth_token.clone();
        match admin {
            None => (Credential::Admin { generation: None }, Principal::Admin),
            Some(t) if crate::server::constant_time_eq(&hello.token, &t) => (
                Credential::Admin {
                    generation: Some(crate::ws::handshake::token_generation(&t)),
                },
                Principal::Admin,
            ),
            Some(_) => {
                warn!("e2e handshake refused: bad gateway token");
                return error(StatusCode::UNAUTHORIZED, "unauthorized");
            }
        }
    };
    if !state
        .e2e
        .first_sight(Sha256::digest(&msg1).into(), now)
        .await
    {
        return error(StatusCode::BAD_REQUEST, "replayed");
    }

    let sid = URL_SAFE_NO_PAD.encode(random_bytes::<16>());
    let expires_at = now + SESSION_TTL_SECS;
    let reply = serde_json::json!({"sid": sid, "expires_at": expires_at, "principal": principal});
    let reply = serde_json::to_vec(&reply).unwrap_or_default();
    let mut msg2 = vec![0u8; reply.len() + 128];
    let Ok(len) = hs.write_message(&reply, &mut msg2) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "e2e_unavailable");
    };
    let (k1, k2) = hs.dangerously_get_raw_split();
    state
        .e2e
        .insert(Session {
            sid,
            credential,
            recv: ChaCha20Poly1305::new(&k1.into()),
            send: ChaCha20Poly1305::new(&k2.into()),
            window: std::sync::Mutex::new(ReplayWindow::default()),
            send_n: AtomicU64::new(0),
            created_at: now,
            expires_at,
        })
        .await;
    Json(serde_json::json!({"v": 1, "m": STANDARD.encode(&msg2[..len])})).into_response()
}

// ---------------------------------------------------------------------------
// Calls and streams
// ---------------------------------------------------------------------------

/// The plaintext of a call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InnerRequest {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// The plaintext of a call's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InnerResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Base64.
    #[serde(default)]
    pub body: String,
}

#[derive(Deserialize)]
pub(crate) struct CallQuery {
    sid: String,
}

#[derive(Deserialize)]
pub(crate) struct Envelope {
    n: u64,
    c: String,
}

#[derive(Deserialize)]
pub(crate) struct EventsQuery {
    sid: String,
    n: u64,
    c: String,
}

/// POST /api/v1/e2e/call?sid=
pub(crate) async fn call(
    State(state): State<Gate>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(q): Query<CallQuery>,
    Json(env): Json<Envelope>,
) -> Response {
    let (session, principal) = match state.e2e.session(&state, &q.sid).await {
        Ok(found) => found,
        Err(resp) => return resp,
    };
    let Ok(ciphertext) = STANDARD.decode(env.c.as_bytes()) else {
        return error(StatusCode::BAD_REQUEST, "bad_envelope");
    };
    let inner = match session
        .open(env.n, &ciphertext)
        .and_then(|plain| parse_inner(&plain))
    {
        Ok(inner) => inner,
        Err(resp) => return resp,
    };
    let request = match inner_request(inner, peer, principal) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let Some(router) = state.e2e.router() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "e2e_unavailable");
    };

    let outcome = tokio::time::timeout(INNER_TIMEOUT, async move {
        let response = router.oneshot(request).await.unwrap_or_else(|e| match e {});
        let streams = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream"));
        if streams {
            return Err("stream_needs_events");
        }
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, MAX_INNER_BODY)
            .await
            .map_err(|_| "response_too_large")?;
        Ok(InnerResponse {
            status: parts.status.as_u16(),
            headers: response_headers(&parts.headers),
            body: STANDARD.encode(body),
        })
    })
    .await;
    let reply = match outcome {
        Ok(Ok(reply)) => reply,
        Ok(Err(code)) => InnerResponse {
            status: StatusCode::BAD_REQUEST.as_u16(),
            headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
            body: STANDARD.encode(serde_json::json!({"error": code}).to_string()),
        },
        Err(_) => InnerResponse {
            status: StatusCode::GATEWAY_TIMEOUT.as_u16(),
            headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
            body: STANDARD.encode(r#"{"error":"timeout"}"#),
        },
    };
    let plain = serde_json::to_vec(&reply).unwrap_or_default();
    Json(session.seal(&plain)).into_response()
}

/// GET /api/v1/e2e/events?sid=&n=&c=
pub(crate) async fn events(
    State(state): State<Gate>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(q): Query<EventsQuery>,
) -> Response {
    let (session, principal) = match state.e2e.session(&state, &q.sid).await {
        Ok(found) => found,
        Err(resp) => return resp,
    };
    let Ok(ciphertext) = URL_SAFE_NO_PAD.decode(q.c.as_bytes()) else {
        return error(StatusCode::BAD_REQUEST, "bad_envelope");
    };
    let inner = match session
        .open(q.n, &ciphertext)
        .and_then(|plain| parse_inner(&plain))
    {
        Ok(inner) => inner,
        Err(resp) => return resp,
    };
    if !inner.method.eq_ignore_ascii_case("GET") || inner.body.is_some() {
        return error(StatusCode::BAD_REQUEST, "events_need_get");
    }
    let request = match inner_request(inner, peer, principal) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let Some(router) = state.e2e.router() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "e2e_unavailable");
    };
    let response = router.oneshot(request).await.unwrap_or_else(|e| match e {});
    let (parts, body) = response.into_parts();
    let head = serde_json::json!({
        "status": parts.status.as_u16(),
        "headers": response_headers(&parts.headers),
    });

    let stream = async_stream::stream! {
        let head = serde_json::to_vec(&head).unwrap_or_default();
        yield Ok::<_, std::convert::Infallible>(
            Event::default().event("head").data(session.seal(&head).to_string()),
        );
        let mut body = body.into_data_stream();
        let mut recheck = tokio::time::interval(STREAM_RECHECK);
        recheck.tick().await;
        loop {
            tokio::select! {
                chunk = body.next() => match chunk {
                    Some(Ok(bytes)) => {
                        yield Ok(Event::default().event("data").data(session.seal(&bytes).to_string()));
                    }
                    _ => break,
                },
                _ = recheck.tick() => {
                    if state.e2e.session(&state, &session.sid).await.is_err() {
                        break;
                    }
                }
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn parse_inner(plain: &[u8]) -> Result<InnerRequest, Response> {
    serde_json::from_slice(plain).map_err(|_| error(StatusCode::BAD_REQUEST, "bad_request"))
}

/// Build the request an envelope carries, marked as coming from a session.
fn inner_request(
    inner: InnerRequest,
    peer: SocketAddr,
    principal: Principal,
) -> Result<Request<Body>, Response> {
    let bad = || error(StatusCode::BAD_REQUEST, "bad_request");
    let method = match inner.method.to_ascii_uppercase().as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        "DELETE" => Method::DELETE,
        _ => return Err(bad()),
    };
    let route = inner.path.split('?').next().unwrap_or_default();
    if !route.starts_with("/api/v1/")
        || route.starts_with("/api/v1/e2e/")
        || route.split('/').any(|seg| seg == "." || seg == "..")
    {
        return Err(bad());
    }
    let body = match inner.body {
        Some(b64) => STANDARD.decode(b64.as_bytes()).map_err(|_| bad())?,
        None => Vec::new(),
    };
    if body.len() > MAX_INNER_BODY {
        return Err(error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large"));
    }
    let mut builder = Request::builder().method(method).uri(inner.path.as_str());
    for (name, value) in &inner.headers {
        let name = name.to_ascii_lowercase();
        if !INNER_HEADERS.contains(&name.as_str()) {
            continue;
        }
        let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(value))
        else {
            return Err(bad());
        };
        builder = builder.header(name, value);
    }
    let mut request = builder.body(Body::from(body)).map_err(|_| bad())?;
    request.extensions_mut().insert(ConnectInfo(peer));
    request.extensions_mut().insert(E2eRequest { principal });
    Ok(request)
}

fn response_headers(headers: &axum::http::HeaderMap) -> BTreeMap<String, String> {
    [header::CONTENT_TYPE, header::ETAG, header::CACHE_CONTROL]
        .iter()
        .filter_map(|name| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|v| (name.as_str().to_owned(), v.to_owned()))
        })
        .collect()
}

/// The app side of the protocol, without I/O: for tests here and in
/// extension crates, and as the reference the app's implementation is
/// checked against.
pub mod client {
    use anyhow::{Context as _, anyhow, bail};

    use super::*;

    /// A fresh device key pair `(private, public)`.
    pub fn device_keypair() -> (Vec<u8>, Vec<u8>) {
        let kp = snow::Builder::new(noise_params())
            .generate_keypair()
            .expect("generate keypair");
        (kp.private, kp.public)
    }

    pub struct Pending {
        hs: snow::HandshakeState,
    }

    /// Start a handshake. Returns the state to finish it with and the
    /// body to POST to `/api/v1/e2e/handshake`.
    pub fn start(
        gateway_pub_b64: &str,
        device_private: &[u8],
        token: &str,
        ts: u64,
    ) -> anyhow::Result<(Pending, serde_json::Value)> {
        let gateway_pub = STANDARD.decode(gateway_pub_b64)?;
        let mut hs = snow::Builder::new(noise_params())
            .local_private_key(device_private)?
            .remote_public_key(&gateway_pub)?
            .prologue(PROTOCOL.as_bytes())?
            .build_initiator()?;
        let hello = serde_json::to_vec(&serde_json::json!({"token": token, "ts": ts}))?;
        let mut msg1 = vec![0u8; hello.len() + 256];
        let len = hs.write_message(&hello, &mut msg1)?;
        Ok((
            Pending { hs },
            serde_json::json!({"v": 1, "m": STANDARD.encode(&msg1[..len])}),
        ))
    }

    impl Pending {
        /// Finish with the gateway's handshake reply.
        pub fn finish(mut self, reply: &serde_json::Value) -> anyhow::Result<ClientSession> {
            let msg2 = STANDARD.decode(reply["m"].as_str().context("no m")?)?;
            let mut payload = vec![0u8; msg2.len()];
            let len = self.hs.read_message(&msg2, &mut payload)?;
            let info: serde_json::Value = serde_json::from_slice(&payload[..len])?;
            let sid = info["sid"].as_str().context("no sid")?.to_owned();
            let (k1, k2) = self.hs.dangerously_get_raw_split();
            Ok(ClientSession {
                sid,
                info,
                send: ChaCha20Poly1305::new(&k1.into()),
                recv: ChaCha20Poly1305::new(&k2.into()),
                next_n: 0,
            })
        }
    }

    pub struct ClientSession {
        pub sid: String,
        /// The msg2 payload: `sid`, `expires_at`, `principal`.
        pub info: serde_json::Value,
        send: ChaCha20Poly1305,
        recv: ChaCha20Poly1305,
        next_n: u64,
    }

    impl ClientSession {
        fn seal(&mut self, plain: &[u8]) -> (u64, Vec<u8>) {
            let n = self.next_n;
            self.next_n += 1;
            (n, encrypt(&self.send, n, self.sid.as_bytes(), plain))
        }

        /// Seal a request with a chosen number, to test replay handling.
        pub fn seal_with(&self, n: u64, inner: &InnerRequest) -> serde_json::Value {
            let plain = serde_json::to_vec(inner).expect("serialize");
            let c = encrypt(&self.send, n, self.sid.as_bytes(), &plain);
            serde_json::json!({"n": n, "c": STANDARD.encode(c)})
        }

        /// The body to POST to `/api/v1/e2e/call?sid=<sid>`.
        pub fn seal_call(&mut self, inner: &InnerRequest) -> serde_json::Value {
            let plain = serde_json::to_vec(inner).expect("serialize");
            let (n, c) = self.seal(&plain);
            serde_json::json!({"n": n, "c": STANDARD.encode(c)})
        }

        /// The full path and query for `GET /api/v1/e2e/events`.
        pub fn events_uri(&mut self, inner_path: &str) -> String {
            let inner = InnerRequest {
                method: "GET".into(),
                path: inner_path.into(),
                headers: BTreeMap::new(),
                body: None,
            };
            let plain = serde_json::to_vec(&inner).expect("serialize");
            let (n, c) = self.seal(&plain);
            format!(
                "/api/v1/e2e/events?sid={}&n={n}&c={}",
                self.sid,
                URL_SAFE_NO_PAD.encode(c)
            )
        }

        /// Open a `{"n","c"}` envelope from the gateway.
        pub fn open(&self, envelope: &serde_json::Value) -> anyhow::Result<Vec<u8>> {
            let n = envelope["n"].as_u64().context("no n")?;
            let c = STANDARD.decode(envelope["c"].as_str().context("no c")?)?;
            decrypt(&self.recv, n, self.sid.as_bytes(), &c).ok_or_else(|| anyhow!("decrypt failed"))
        }

        /// Open a call reply.
        pub fn open_reply(&self, envelope: &serde_json::Value) -> anyhow::Result<InnerResponse> {
            if envelope.get("error").is_some() {
                bail!("gateway refused the call: {envelope}");
            }
            Ok(serde_json::from_slice(&self.open(envelope)?)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::{
        middleware::Next,
        routing::{get, post},
    };
    use rsclaw_config::{
        runtime::GatewayRuntime,
        schema::{BindMode, GatewayMode, ReloadMode},
    };

    const ADMIN: &str = "admin-token";

    fn gateway_runtime() -> GatewayRuntime {
        GatewayRuntime {
            port: 18888,
            mode: GatewayMode::Local,
            bind: BindMode::Loopback,
            bind_address: None,
            reload: ReloadMode::Hybrid,
            auth_token: Some(ADMIN.into()),
            a2a_principals: vec![],
            a2a_relay: Default::default(),
            a2a_max_body_bytes: 1024,
            allow_tailscale: false,
            channel_health_check_minutes: 5,
            channel_stale_event_threshold_minutes: 30,
            channel_max_restarts_per_hour: 10,
            auth_token_configured: true,
            auth_token_is_plaintext: true,
            user_agent: None,
            language: None,
        }
    }

    /// The part of the gateway's auth middleware this module relies on.
    async fn test_auth(mut request: axum::extract::Request, next: Next) -> Response {
        if let Some(admitted) = admit(&mut request) {
            return match admitted {
                Ok(()) => next.run(request).await,
                Err(refused) => refused,
            };
        }
        let path = request.uri().path().to_owned();
        if matches!(
            path.as_str(),
            "/api/v1/e2e/handshake" | "/api/v1/e2e/call" | "/api/v1/e2e/events"
        ) {
            return next.run(request).await;
        }
        if crate::ext::e2e_only(&path) {
            return error(StatusCode::FORBIDDEN, "e2e_required");
        }
        let bearer = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if bearer != Some(ADMIN) {
            return error(StatusCode::UNAUTHORIZED, "unauthorized");
        }
        request.extensions_mut().insert(Principal::Admin);
        next.run(request).await
    }

    async fn who(
        principal: Option<axum::Extension<Principal>>,
        body: axum::body::Bytes,
    ) -> Response {
        Json(serde_json::json!({
            "principal": principal.map(|p| p.0),
            "echo": String::from_utf8_lossy(&body),
        }))
        .into_response()
    }

    async fn ticks() -> Response {
        let stream = futures::stream::iter((0..3).map(|i| {
            Ok::<_, std::convert::Infallible>(
                Event::default().id(i.to_string()).data(format!("tick{i}")),
            )
        }));
        Sse::new(stream).into_response()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        gate: Gate,
        router: Router,
    }

    fn fixture() -> Fixture {
        crate::ext::register_scoped_area(crate::ext::ScopedArea {
            name: "testarea",
            actions: &["read", "write"],
            e2e_only: true,
        });
        let dir = tempfile::tempdir().expect("test");
        let gate = Gate {
            e2e: Arc::new(E2e::ephemeral()),
            tokens: Arc::new(AccessTokens::load(dir.path().join("tokens.json"))),
            gateway: Arc::new(tokio::sync::RwLock::new(gateway_runtime())),
        };
        let api = Router::new()
            .route("/e2e/handshake", post(handshake))
            .route("/e2e/call", post(call))
            .route("/e2e/events", get(events))
            .route("/e2e/key", get(public_key))
            .route("/tokens", post(crate::access::mint_token))
            .route(
                "/tokens/{id}",
                axum::routing::delete(crate::access::revoke_token),
            )
            .route("/testarea/who", get(who).post(who))
            .route("/testarea/ticks", get(ticks))
            .route("/config", get(who))
            .with_state(gate.clone());
        let router = Router::new()
            .nest("/api/v1", api)
            .layer(axum::middleware::from_fn(test_auth));
        gate.e2e.set_router(router.clone());
        Fixture {
            _dir: dir,
            gate,
            router,
        }
    }

    async fn send(router: &Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
        let mut request = request;
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        let response = router.clone().oneshot(request).await.expect("test");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("test");
        (status, body.to_vec())
    }

    fn post_json(uri: &str, body: &serde_json::Value, bearer: Option<&str>) -> Request<Body> {
        let mut b = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
        if let Some(t) = bearer {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::from(body.to_string())).expect("test")
    }

    fn json(bytes: &[u8]) -> serde_json::Value {
        serde_json::from_slice(bytes).expect("test")
    }

    async fn open_session(
        f: &Fixture,
        token: &str,
        device: &[u8],
    ) -> Result<client::ClientSession, (StatusCode, serde_json::Value)> {
        let (pending, body) = client::start(
            &f.gate.e2e.public_key_b64().expect("test"),
            device,
            token,
            now_secs(),
        )
        .expect("test");
        let (status, reply) =
            send(&f.router, post_json("/api/v1/e2e/handshake", &body, None)).await;
        if status != StatusCode::OK {
            return Err((status, json(&reply)));
        }
        Ok(pending.finish(&json(&reply)).expect("test"))
    }

    async fn call_inner(
        f: &Fixture,
        s: &mut client::ClientSession,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> InnerResponse {
        let env = s.seal_call(&InnerRequest {
            method: method.into(),
            path: path.into(),
            headers: BTreeMap::new(),
            body: body.map(|b| STANDARD.encode(b)),
        });
        let (status, reply) = send(
            &f.router,
            post_json(&format!("/api/v1/e2e/call?sid={}", s.sid), &env, None),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&reply)
        );
        s.open_reply(&json(&reply)).expect("test")
    }

    fn inner_json(r: &InnerResponse) -> serde_json::Value {
        json(&STANDARD.decode(&r.body).expect("test"))
    }

    async fn mint(f: &Fixture, scopes: &[&str]) -> (String, String) {
        let (status, body) = send(
            &f.router,
            post_json(
                "/api/v1/tokens",
                &serde_json::json!({"label": "front desk", "scopes": scopes}),
                Some(ADMIN),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let v = json(&body);
        assert_eq!(v["e2e_pub"], serde_json::json!(f.gate.e2e.public_key_b64()));
        (
            v["token"].as_str().expect("test").into(),
            v["info"]["id"].as_str().expect("test").into(),
        )
    }

    #[tokio::test]
    async fn plaintext_never_reaches_an_e2e_only_area() {
        let f = fixture();
        let get = |bearer: Option<&str>| {
            let mut b = Request::get("/api/v1/testarea/who");
            if let Some(t) = bearer {
                b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
            }
            b.body(Body::empty()).expect("test")
        };
        let (status, body) = send(&f.router, get(Some(ADMIN))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json(&body)["error"], "e2e_required");
        let (token, _) = mint(&f, &["testarea:read"]).await;
        assert_eq!(
            send(&f.router, get(Some(&token))).await.0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn scoped_session_reaches_its_area_and_nothing_else() {
        let f = fixture();
        let (token, id) = mint(&f, &["testarea:read"]).await;
        let (device, _) = client::device_keypair();
        let mut s = open_session(&f, &token, &device).await.expect("test");
        assert_eq!(s.info["principal"]["kind"], "scoped");
        assert_eq!(s.info["principal"]["label"], "front desk");

        let r = call_inner(&f, &mut s, "POST", "/api/v1/testarea/who", Some("hello")).await;
        assert_eq!(r.status, 200);
        let v = inner_json(&r);
        assert_eq!(v["principal"]["token_id"], id);
        assert_eq!(v["echo"], "hello");

        for path in ["/api/v1/config", "/api/v1/tokens"] {
            let r = call_inner(&f, &mut s, "GET", path, None).await;
            assert_eq!(r.status, 403, "{path}");
            assert_eq!(inner_json(&r)["error"], "out_of_scope");
        }

        // A second phone cannot use the same token.
        let (other, _) = client::device_keypair();
        let (status, body) = open_session(&f, &token, &other).await.err().expect("test");
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "device_mismatch");
    }

    #[tokio::test]
    async fn admin_session_reaches_everything() {
        let f = fixture();
        let (device, _) = client::device_keypair();
        let mut s = open_session(&f, ADMIN, &device).await.expect("test");
        assert_eq!(s.info["principal"]["kind"], "admin");
        assert_eq!(
            call_inner(&f, &mut s, "GET", "/api/v1/testarea/who", None)
                .await
                .status,
            200
        );
        assert_eq!(
            call_inner(&f, &mut s, "GET", "/api/v1/config", None)
                .await
                .status,
            200
        );
        let (status, _) = open_session(&f, "wrong", &device)
            .await
            .err()
            .expect("test");
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Rotating the gateway token ends the session.
        f.gate.gateway.write().await.auth_token = Some("rotated".into());
        let env = s.seal_call(&InnerRequest {
            method: "GET".into(),
            path: "/api/v1/config".into(),
            headers: BTreeMap::new(),
            body: None,
        });
        let (status, body) = send(
            &f.router,
            post_json(&format!("/api/v1/e2e/call?sid={}", s.sid), &env, None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json(&body)["error"], "session_revoked");
    }

    #[tokio::test]
    async fn replays_and_forgeries_are_refused() {
        let f = fixture();
        let (device, _) = client::device_keypair();

        // A handshake is accepted once.
        let (pending, body) = client::start(
            &f.gate.e2e.public_key_b64().expect("test"),
            &device,
            ADMIN,
            now_secs(),
        )
        .expect("test");
        let (status, reply) =
            send(&f.router, post_json("/api/v1/e2e/handshake", &body, None)).await;
        assert_eq!(status, StatusCode::OK);
        let (status, again) =
            send(&f.router, post_json("/api/v1/e2e/handshake", &body, None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json(&again)["error"], "replayed");
        let s = pending.finish(&json(&reply)).expect("test");

        // A stale handshake is refused.
        let (_, stale) = client::start(
            &f.gate.e2e.public_key_b64().expect("test"),
            &device,
            ADMIN,
            now_secs() - 3600,
        )
        .expect("test");
        let (status, body) =
            send(&f.router, post_json("/api/v1/e2e/handshake", &stale, None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json(&body)["error"], "stale_handshake");

        let inner = InnerRequest {
            method: "GET".into(),
            path: "/api/v1/config".into(),
            headers: BTreeMap::new(),
            body: None,
        };
        let uri = format!("/api/v1/e2e/call?sid={}", s.sid);
        let env = s.seal_with(7, &inner);
        assert_eq!(
            send(&f.router, post_json(&uri, &env, None)).await.0,
            StatusCode::OK
        );
        let (status, body) = send(&f.router, post_json(&uri, &env, None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json(&body)["error"], "replayed");

        // Tampered ciphertext, or the right ciphertext under another number.
        let mut tampered = s.seal_with(8, &inner);
        let mut c = STANDARD
            .decode(tampered["c"].as_str().expect("test"))
            .expect("test");
        c[0] ^= 1;
        tampered["c"] = STANDARD.encode(c).into();
        assert_eq!(
            send(&f.router, post_json(&uri, &tampered, None)).await.0,
            StatusCode::BAD_REQUEST
        );
        let mut renumbered = s.seal_with(9, &inner);
        renumbered["n"] = 10.into();
        assert_eq!(
            send(&f.router, post_json(&uri, &renumbered, None)).await.0,
            StatusCode::BAD_REQUEST
        );

        // Unknown session.
        let (status, body) = send(
            &f.router,
            post_json("/api/v1/e2e/call?sid=nope", &env, None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json(&body)["error"], "session_expired");
    }

    #[tokio::test]
    async fn revoking_a_token_ends_its_sessions() {
        let f = fixture();
        let (token, id) = mint(&f, &["testarea:read"]).await;
        let (device, _) = client::device_keypair();
        let mut s = open_session(&f, &token, &device).await.expect("test");
        assert_eq!(
            call_inner(&f, &mut s, "GET", "/api/v1/testarea/who", None)
                .await
                .status,
            200
        );
        let revoke = Request::delete(format!("/api/v1/tokens/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN}"))
            .body(Body::empty())
            .expect("test");
        assert_eq!(send(&f.router, revoke).await.0, StatusCode::NO_CONTENT);
        assert!(!f.gate.e2e.has_session(&s.sid).await);
        let env = s.seal_call(&InnerRequest {
            method: "GET".into(),
            path: "/api/v1/testarea/who".into(),
            headers: BTreeMap::new(),
            body: None,
        });
        let (status, _) = send(
            &f.router,
            post_json(&format!("/api/v1/e2e/call?sid={}", s.sid), &env, None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn events_carry_an_encrypted_stream() {
        let f = fixture();
        let (token, _) = mint(&f, &["testarea:read"]).await;
        let (device, _) = client::device_keypair();
        let mut s = open_session(&f, &token, &device).await.expect("test");

        // A stream through /call is refused rather than hanging.
        let r = call_inner(&f, &mut s, "GET", "/api/v1/testarea/ticks", None).await;
        assert_eq!(inner_json(&r)["error"], "stream_needs_events");

        let uri = s.events_uri("/api/v1/testarea/ticks");
        let (status, body) = send(
            &f.router,
            Request::get(uri).body(Body::empty()).expect("test"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(body).expect("test");
        assert!(!text.contains("tick"), "plaintext leaked: {text}");
        let mut events = Vec::new();
        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                events.push(
                    s.open(&serde_json::from_str(data).expect("test"))
                        .expect("test"),
                );
            }
        }
        let head = json(&events[0]);
        assert_eq!(head["status"], 200);
        assert!(
            head["headers"]["content-type"]
                .as_str()
                .expect("test")
                .starts_with("text/event-stream")
        );
        let inner: String = events[1..]
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        assert!(inner.contains("id: 0\ndata: tick0"), "{inner}");
        assert!(inner.contains("data: tick2"), "{inner}");
    }

    #[test]
    fn replay_window_refuses_repeats_and_stragglers() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(5));
        assert!(!w.accept(5));
        assert!(w.accept(3));
        assert!(w.accept(2000));
        assert!(!w.accept(2000 - REPLAY_WINDOW));
        assert!(w.accept(2000 - REPLAY_WINDOW + 1));
        assert!(!w.accept(3));
        assert!(w.seen.len() <= REPLAY_WINDOW as usize + 1);
    }

    #[test]
    fn handshake_keys_agree_between_client_and_gateway() {
        let gw = E2e::ephemeral();
        let identity = gw.identity.as_ref().expect("test");
        let (dev_priv, dev_pub) = client::device_keypair();
        let (pending, body) =
            client::start(&gw.public_key_b64().expect("test"), &dev_priv, "tok", 1).expect("test");

        let msg1 = STANDARD
            .decode(body["m"].as_str().expect("test"))
            .expect("test");
        let mut hs = snow::Builder::new(noise_params())
            .local_private_key(&identity.private)
            .expect("test")
            .prologue(PROTOCOL.as_bytes())
            .expect("test")
            .build_responder()
            .expect("test");
        let mut buf = vec![0u8; msg1.len()];
        let len = hs.read_message(&msg1, &mut buf).expect("test");
        assert_eq!(&buf[..len], br#"{"token":"tok","ts":1}"#);
        assert_eq!(hs.get_remote_static().expect("test"), &dev_pub[..]);
        let reply = br#"{"sid":"abc","expires_at":9}"#;
        let mut msg2 = vec![0u8; 256];
        let len = hs.write_message(reply, &mut msg2).expect("test");
        let (k1, k2) = hs.dangerously_get_raw_split();
        let mut session = pending
            .finish(&serde_json::json!({"m": STANDARD.encode(&msg2[..len])}))
            .expect("test");
        assert_eq!(session.sid, "abc");

        let gw_recv = ChaCha20Poly1305::new(&k1.into());
        let gw_send = ChaCha20Poly1305::new(&k2.into());
        let env = session.seal_call(&InnerRequest {
            method: "GET".into(),
            path: "/api/v1/health".into(),
            headers: BTreeMap::new(),
            body: None,
        });
        let c = STANDARD
            .decode(env["c"].as_str().expect("test"))
            .expect("test");
        assert!(decrypt(&gw_recv, 0, b"abc", &c).is_some());
        assert!(decrypt(&gw_recv, 1, b"abc", &c).is_none());
        assert!(decrypt(&gw_recv, 0, b"abd", &c).is_none());
        let back = encrypt(&gw_send, 0, b"abc", b"hi");
        let opened = session
            .open(&serde_json::json!({"n": 0, "c": STANDARD.encode(back)}))
            .expect("test");
        assert_eq!(opened, b"hi");
    }
}
