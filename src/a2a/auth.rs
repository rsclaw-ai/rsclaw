//! A2A auth middleware — one credential pool, accepted on either the
//! `Authorization: Bearer` or `X-API-Key` header (transport is the caller's
//! choice, not a config axis).
//!
//! Credentials resolve to a [`crate::config::runtime::A2aPrincipal`] at startup
//! (`gateway.a2a.clients` → named principals; the deprecated
//! `authTokens`/`apiKeys` and env lists → anonymous principals), plus
//! `gateway.auth.token` merged live as the `gateway-auth` principal. The
//! middleware only consults the live config, so it is reload-safe.
//!
//! On a successful match the resolved [`A2aIdentity`] (id + scopes, no secret)
//! is inserted into the request extensions so downstream handlers can attribute
//! and authorize the call (A2A spec §7.5; scope enforcement is a follow-up).
//!
//! When the pool is empty the middleware passes everything through (dev mode).

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};

use crate::config::runtime::A2aPrincipal;
use crate::server::{AppState, constant_time_eq};

/// Resolved identity for an authenticated A2A request, inserted into the
/// request extensions. Carries the principal id and scopes but NOT the secret,
/// so downstream handlers can attribute / authorize without re-exposing it.
#[derive(Debug, Clone)]
pub struct A2aIdentity {
    pub id: String,
    pub scopes: Vec<String>,
}

/// Snapshot of accepted A2A credentials, materialised once per request.
struct Accepted {
    principals: Vec<A2aPrincipal>,
}

impl Accepted {
    fn is_empty(&self) -> bool {
        self.principals.is_empty()
    }

    /// Constant-time match of a presented secret against every principal. We
    /// scan ALL principals (no early break) so the response time doesn't leak
    /// how many credentials are configured or where a match landed.
    fn match_secret(&self, presented: &str) -> Option<A2aIdentity> {
        let mut matched: Option<&A2aPrincipal> = None;
        for p in &self.principals {
            if constant_time_eq(&p.secret, presented) {
                matched = Some(p);
            }
        }
        matched.map(|p| A2aIdentity {
            id: p.id.clone(),
            scopes: p.scopes.clone(),
        })
    }
}

/// Build the per-request accepted-credentials set from live state. Principals
/// are resolved at startup; `gateway.auth.token` is merged live as the
/// `gateway-auth` principal so a single configured token unifies all auth.
async fn collect_accepted(state: &AppState) -> Accepted {
    let gw = state.live.gateway.read().await;
    let mut principals = gw.a2a_principals.clone();
    if let Some(unified) = gw.auth_token.as_ref() {
        if !principals.iter().any(|p| &p.secret == unified) {
            principals.push(A2aPrincipal {
                id: "gateway-auth".to_owned(),
                secret: unified.clone(),
                scopes: Vec::new(),
            });
        }
    }
    Accepted { principals }
}

pub async fn a2a_auth_layer(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let accepted = collect_accepted(&state).await;
    if accepted.is_empty() {
        // Dev pass-through — nothing configured anywhere.
        return Ok(next.run(req).await);
    }

    // A secret on EITHER header is valid; transport is the caller's choice.
    let presented = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| req.headers().get("x-api-key").and_then(|v| v.to_str().ok()));

    if let Some(secret) = presented
        && let Some(identity) = accepted.match_secret(secret)
    {
        // Attribution: downstream handlers read this to authorize / audit.
        req.extensions_mut().insert(identity);
        return Ok(next.run(req).await);
    }

    Err(StatusCode::UNAUTHORIZED)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `Accepted` from `(id, secret)` pairs.
    fn accepted(pairs: &[(&str, &str)]) -> Accepted {
        Accepted {
            principals: pairs
                .iter()
                .map(|(id, secret)| A2aPrincipal {
                    id: (*id).to_owned(),
                    secret: (*secret).to_owned(),
                    scopes: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn empty_accepted_is_dev_passthrough_marker() {
        assert!(accepted(&[]).is_empty());
        assert!(!accepted(&[("a", "x")]).is_empty());
    }

    #[test]
    fn match_resolves_to_principal_identity() {
        let a = accepted(&[("partner-acme", "sk-acme"), ("a1", "sk-a1")]);
        // A correct secret resolves to the owning principal — this is the
        // attribution that A2A §7.5 authorization needs.
        assert_eq!(a.match_secret("sk-acme").unwrap().id, "partner-acme");
        assert_eq!(a.match_secret("sk-a1").unwrap().id, "a1");
        // Unknown secret → no identity.
        assert!(a.match_secret("sk-nope").is_none());
    }

    #[test]
    fn match_is_constant_time_length_safe() {
        let a = accepted(&[("a", "right")]);
        assert!(a.match_secret("right").is_some());
        assert!(a.match_secret("wrong").is_none());
        // Length mismatch is rejected by constant_time_eq.
        assert!(a.match_secret("rightandmore").is_none());
    }

    #[test]
    fn one_secret_matches_regardless_of_header() {
        // The pool is header-agnostic: the same secret authenticates whether it
        // arrives as a Bearer token or an X-API-Key (the layer tries both
        // headers against this one pool).
        let a = accepted(&[("client", "shared-secret")]);
        assert_eq!(a.match_secret("shared-secret").unwrap().id, "client");
    }
}
