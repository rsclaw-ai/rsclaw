//! Scoped access tokens: credentials narrower than the gateway token.
//!
//! The gateway token can do everything, so it must not be handed to every
//! person or phone that only needs one feature. An admin mints a scoped
//! token instead: a label (who holds it), a list of scopes such as
//! `inbox:read`, optionally the accounts it may see, and an optional expiry.
//!
//! A scoped token never works as a plain `Authorization` header. It is
//! presented only inside an end-to-end session handshake (see
//! [`crate::e2e`]); the first handshake binds the token to that device's
//! key, and a session it opens reaches only `/api/v1/<area>/...` for the
//! areas its scopes name. Areas exist only when an extension registers them
//! ([`crate::ext::register_scoped_area`]), so a scope can never open a
//! built-in admin API.
//!
//! The gateway stores a SHA-256 of each token, never the token itself, and
//! shows the token once, when it is minted. Rotating the gateway token does
//! not touch scoped tokens, and revoking a scoped token does not touch the
//! gateway token.

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::warn;

use crate::e2e::Gate;

/// Prefix of every scoped token, so the gateway can tell one from the
/// gateway token without a lookup.
pub const TOKEN_PREFIX: &str = "rsat_";

/// Who a request acts for. Inserted into every authenticated request's
/// extensions, so extension routes can read it with
/// `Option<axum::Extension<Principal>>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    /// The gateway token, or a gateway that runs without one.
    Admin,
    /// A scoped token.
    Scoped(ScopedPrincipal),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopedPrincipal {
    pub token_id: String,
    pub label: String,
    pub scopes: Vec<String>,
    /// Accounts this token may see; `None` means all of them.
    pub accounts: Option<Vec<String>>,
}

impl Principal {
    /// True when the principal holds `scope` (the admin holds every scope).
    pub fn has_scope(&self, scope: &str) -> bool {
        match self {
            Self::Admin => true,
            Self::Scoped(p) => p.scopes.iter().any(|s| s == scope),
        }
    }

    /// True when the principal may see the account keyed `account`.
    pub fn allows_account(&self, account: &str) -> bool {
        match self {
            Self::Admin => true,
            Self::Scoped(p) => p
                .accounts
                .as_ref()
                .is_none_or(|keys| keys.iter().any(|k| k == account)),
        }
    }

    /// The token's label; `None` for the admin.
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Admin => None,
            Self::Scoped(p) => Some(&p.label),
        }
    }

    /// True when the principal may call `path`. A scoped principal reaches
    /// the health check and the areas its scopes name, nothing else.
    pub(crate) fn may_reach(&self, path: &str) -> bool {
        let Self::Scoped(p) = self else {
            return true;
        };
        if path == "/api/v1/health" {
            return true;
        }
        let Some(rest) = path.strip_prefix("/api/v1/") else {
            return false;
        };
        let area = rest.split('/').next().unwrap_or_default();
        crate::ext::scoped_area(area).is_some()
            && p.scopes.iter().any(|s| scope_area(s) == Some(area))
    }
}

fn scope_area(scope: &str) -> Option<&str> {
    scope.split_once(':').map(|(area, _)| area)
}

/// Why a scoped token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessError {
    Unknown,
    Expired,
    /// The token is bound to a different device key.
    OtherDevice,
}

impl AccessError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unknown => "unauthorized",
            Self::Expired => "token_expired",
            Self::OtherDevice => "device_mismatch",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenRecord {
    id: String,
    label: String,
    scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accounts: Option<Vec<String>>,
    /// Hex SHA-256 of the whole token.
    hash: String,
    created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    /// Base64 X25519 public key of the device the token is bound to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_used_at: Option<u64>,
}

impl TokenRecord {
    fn principal(&self) -> ScopedPrincipal {
        ScopedPrincipal {
            token_id: self.id.clone(),
            label: self.label.clone(),
            scopes: self.scopes.clone(),
            accounts: self.accounts.clone(),
        }
    }

    fn live(&self, now: u64) -> bool {
        self.expires_at.is_none_or(|exp| now < exp)
    }

    fn view(&self) -> TokenView {
        TokenView {
            id: self.id.clone(),
            label: self.label.clone(),
            scopes: self.scopes.clone(),
            accounts: self.accounts.clone(),
            created_at: self.created_at,
            expires_at: self.expires_at,
            device_bound: self.device_key.is_some(),
            last_used_at: self.last_used_at,
        }
    }
}

/// A token as the admin API shows it: everything but the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenView {
    pub id: String,
    pub label: String,
    pub scopes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounts: Option<Vec<String>>,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    pub device_bound: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
}

/// Body of `POST /api/v1/tokens`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintRequest {
    pub label: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub accounts: Option<Vec<String>>,
    #[serde(default)]
    pub expires_at: Option<u64>,
}

const MAX_LABEL_CHARS: usize = 64;
const MAX_TOKENS: usize = 256;

/// The scoped tokens of this gateway, persisted as JSON (mode 0600).
pub struct AccessTokens {
    records: RwLock<Vec<TokenRecord>>,
    path: PathBuf,
}

impl AccessTokens {
    /// Load the store from `path`; a missing file is an empty store. A file
    /// that cannot be parsed is also treated as empty, and left in place
    /// until the next mint or revoke rewrites it.
    pub fn load(path: PathBuf) -> Self {
        let records = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                warn!(error = %e, path = %path.display(), "access token store unreadable; starting empty");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        Self {
            records: RwLock::new(records),
            path,
        }
    }

    /// Mint a token. Returns its view and the token itself, which is never
    /// shown again.
    pub async fn mint(&self, req: MintRequest, now: u64) -> Result<(TokenView, String), String> {
        let label = req.label.trim().to_owned();
        if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
            return Err(format!("label must be 1-{MAX_LABEL_CHARS} characters"));
        }
        if req.scopes.is_empty() {
            return Err("at least one scope is required".into());
        }
        let mut scopes = Vec::new();
        for scope in &req.scopes {
            check_scope(scope)?;
            if !scopes.contains(scope) {
                scopes.push(scope.clone());
            }
        }
        let accounts = match req.accounts {
            Some(keys) if keys.iter().any(|k| k.trim().is_empty()) => {
                return Err("account keys must not be empty".into());
            }
            other => other,
        };
        if req.expires_at.is_some_and(|exp| exp <= now) {
            return Err("expires_at is in the past".into());
        }

        let id = random_hex(6);
        let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<32>());
        let token = format!("{TOKEN_PREFIX}{id}_{secret}");
        let record = TokenRecord {
            id,
            label,
            scopes,
            accounts,
            hash: sha256_hex(&token),
            created_at: now,
            expires_at: req.expires_at,
            device_key: None,
            last_used_at: None,
        };
        let view = record.view();
        {
            let mut records = self.records.write().await;
            records.retain(|r| r.live(now));
            if records.len() >= MAX_TOKENS {
                return Err(format!("at most {MAX_TOKENS} tokens"));
            }
            records.push(record);
        }
        self.persist().await;
        Ok((view, token))
    }

    pub async fn list(&self) -> Vec<TokenView> {
        self.records
            .read()
            .await
            .iter()
            .map(TokenRecord::view)
            .collect()
    }

    /// Revoke a token by id. Returns whether it existed.
    pub async fn revoke(&self, id: &str) -> bool {
        let removed = {
            let mut records = self.records.write().await;
            let before = records.len();
            records.retain(|r| r.id != id);
            records.len() != before
        };
        if removed {
            self.persist().await;
        }
        removed
    }

    /// Check `token` presented by the device holding `device_key`. The first
    /// successful check binds the token to that key; later checks from any
    /// other key fail.
    pub async fn authenticate(
        &self,
        token: &str,
        device_key: &[u8],
        now: u64,
    ) -> Result<ScopedPrincipal, AccessError> {
        let id = token
            .strip_prefix(TOKEN_PREFIX)
            .and_then(|rest| rest.split_once('_'))
            .map(|(id, _)| id)
            .ok_or(AccessError::Unknown)?;
        let hash = sha256_hex(token);
        let device = base64::engine::general_purpose::STANDARD.encode(device_key);
        let principal = {
            let mut records = self.records.write().await;
            let record = records
                .iter_mut()
                .find(|r| r.id == id && crate::server::constant_time_eq(&r.hash, &hash))
                .ok_or(AccessError::Unknown)?;
            if !record.live(now) {
                return Err(AccessError::Expired);
            }
            match &record.device_key {
                Some(bound) if *bound != device => return Err(AccessError::OtherDevice),
                Some(_) => {}
                None => record.device_key = Some(device),
            }
            record.last_used_at = Some(now);
            record.principal()
        };
        self.persist().await;
        Ok(principal)
    }

    /// The principal of token `id` if it is still valid, so a session can
    /// notice a revoke or an expiry without the token itself.
    pub async fn current(&self, id: &str, now: u64) -> Option<ScopedPrincipal> {
        self.records
            .read()
            .await
            .iter()
            .find(|r| r.id == id && r.live(now))
            .map(TokenRecord::principal)
    }

    async fn persist(&self) {
        let json = {
            let records = self.records.read().await;
            match serde_json::to_string_pretty(&*records) {
                Ok(json) => json,
                Err(e) => {
                    warn!(error = %e, "access token store serialize failed");
                    return;
                }
            }
        };
        if let Err(e) = write_private(&self.path, json.as_bytes()).await {
            warn!(error = %e, path = %self.path.display(), "access token store write failed");
        }
    }
}

/// A scope is `<area>:<action>` for an area an extension registered, and
/// one of that area's actions.
fn check_scope(scope: &str) -> Result<(), String> {
    let Some((area, action)) = scope.split_once(':') else {
        return Err(format!("scope {scope:?} must be <area>:<action>"));
    };
    let Some(registered) = crate::ext::scoped_area(area) else {
        return Err(format!("unknown scope area {area:?}"));
    };
    if !registered.actions.contains(&action) {
        return Err(format!("unknown scope {scope:?}"));
    }
    Ok(())
}

/// Write `bytes` to `path` through a temp file, owner-only on unix.
pub(crate) async fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    tokio::fs::rename(&tmp, path).await
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    rand::fill(&mut out);
    out
}

fn random_hex(n: usize) -> String {
    random_bytes::<16>()[..n]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({"error": msg.into()}))).into_response()
}

// ---------------------------------------------------------------------------
// Admin routes. Scoped principals never reach them: `tokens` is not an area
// an extension can register.
// ---------------------------------------------------------------------------

/// POST /api/v1/tokens
pub(crate) async fn mint_token(
    State(state): State<Gate>,
    Json(req): Json<MintRequest>,
) -> Response {
    match state.tokens.mint(req, now_secs()).await {
        Ok((view, token)) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "token": token,
                "e2e_pub": state.e2e.public_key_b64(),
                "info": view,
            })),
        )
            .into_response(),
        Err(msg) => error(StatusCode::BAD_REQUEST, msg),
    }
}

/// GET /api/v1/tokens
pub(crate) async fn list_tokens(State(state): State<Gate>) -> Response {
    Json(serde_json::json!({"tokens": state.tokens.list().await})).into_response()
}

/// DELETE /api/v1/tokens/{id}
pub(crate) async fn revoke_token(State(state): State<Gate>, Path(id): Path<String>) -> Response {
    if !state.tokens.revoke(&id).await {
        return error(StatusCode::NOT_FOUND, "no such token");
    }
    state.e2e.drop_sessions_of(&id).await;
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register_test_area() {
        crate::ext::register_scoped_area(crate::ext::ScopedArea {
            name: "testarea",
            actions: &["read", "write"],
            e2e_only: true,
        });
    }

    fn store() -> (tempfile::TempDir, AccessTokens) {
        register_test_area();
        let dir = tempfile::tempdir().expect("test");
        let store = AccessTokens::load(dir.path().join("tokens.json"));
        (dir, store)
    }

    fn req(scopes: &[&str]) -> MintRequest {
        MintRequest {
            label: "front desk".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            accounts: None,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn mint_checks_scopes_against_registered_areas() {
        let (_dir, store) = store();
        assert!(store.mint(req(&["testarea:read"]), 10).await.is_ok());
        assert!(store.mint(req(&["testarea:delete"]), 10).await.is_err());
        assert!(store.mint(req(&["config:read"]), 10).await.is_err());
        assert!(store.mint(req(&["tokens:read"]), 10).await.is_err());
        assert!(store.mint(req(&[]), 10).await.is_err());
    }

    #[tokio::test]
    async fn first_device_binds_the_token() {
        let (dir, store) = store();
        let (view, token) = store.mint(req(&["testarea:read"]), 10).await.expect("test");
        assert!(token.starts_with(TOKEN_PREFIX));
        let p = store
            .authenticate(&token, &[1; 32], 11)
            .await
            .expect("test");
        assert_eq!(p.token_id, view.id);
        assert!(store.authenticate(&token, &[1; 32], 12).await.is_ok());
        assert_eq!(
            store.authenticate(&token, &[2; 32], 12).await,
            Err(AccessError::OtherDevice)
        );
        assert_eq!(
            store.authenticate(&format!("{token}x"), &[1; 32], 12).await,
            Err(AccessError::Unknown)
        );

        // The binding and the hash survive a reload; the token does not.
        let raw = std::fs::read_to_string(dir.path().join("tokens.json")).expect("test");
        assert!(!raw.contains(&token));
        let reloaded = AccessTokens::load(dir.path().join("tokens.json"));
        assert_eq!(
            reloaded.authenticate(&token, &[2; 32], 13).await,
            Err(AccessError::OtherDevice)
        );
    }

    #[tokio::test]
    async fn expiry_and_revoke_end_a_token() {
        let (_dir, store) = store();
        let mut r = req(&["testarea:read"]);
        r.expires_at = Some(100);
        let (view, token) = store.mint(r, 10).await.expect("test");
        assert!(store.current(&view.id, 50).await.is_some());
        assert_eq!(
            store.authenticate(&token, &[1; 32], 100).await,
            Err(AccessError::Expired)
        );
        assert!(store.current(&view.id, 100).await.is_none());

        let (view, token) = store.mint(req(&["testarea:read"]), 10).await.expect("test");
        assert!(store.revoke(&view.id).await);
        assert!(!store.revoke(&view.id).await);
        assert_eq!(
            store.authenticate(&token, &[1; 32], 11).await,
            Err(AccessError::Unknown)
        );
    }

    #[test]
    fn scoped_principal_reaches_only_its_areas() {
        register_test_area();
        let p = Principal::Scoped(ScopedPrincipal {
            token_id: "t".into(),
            label: "l".into(),
            scopes: vec!["testarea:read".into()],
            accounts: Some(vec!["acct-a".into()]),
        });
        assert!(p.may_reach("/api/v1/testarea/things"));
        assert!(p.may_reach("/api/v1/testarea"));
        assert!(p.may_reach("/api/v1/health"));
        assert!(!p.may_reach("/api/v1/testareax/things"));
        assert!(!p.may_reach("/api/v1/config"));
        assert!(!p.may_reach("/api/v1/tokens"));
        assert!(!p.may_reach("/v1/chat/completions"));
        assert!(p.has_scope("testarea:read"));
        assert!(!p.has_scope("testarea:write"));
        assert!(p.allows_account("acct-a"));
        assert!(!p.allows_account("acct-b"));
        assert!(Principal::Admin.may_reach("/api/v1/config"));
    }
}
