//! A2A auth middleware — one credential pool, accepted on either the
//! `Authorization: Bearer` or `X-API-Key` header (transport is the caller's
//! choice, not a config axis).
//!
//! Credentials resolve to a [`rsclaw_config::runtime::A2aPrincipal`] at startup
//! (`gateway.a2a.clients` → named principals; the deprecated
//! `authTokens`/`apiKeys` and env lists → anonymous principals), plus
//! `gateway.auth.token` merged live as the `gateway-auth` principal. The
//! middleware only consults the live config, so it is reload-safe.
//!
//! On a successful match the resolved [`A2aIdentity`] (id + scopes, no secret)
//! is inserted into the request extensions so downstream handlers can attribute
//! and authorize the call. Standard inbound JSON-RPC methods enforce the
//! principal's configured scopes before dispatch (A2A spec §7.5).
//!
//! When the pool is empty the middleware passes everything through (dev mode).

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use rsclaw_config::runtime::A2aPrincipal;

use crate::server::{AppState, constant_time_eq};

/// Resolved identity for an authenticated A2A request, inserted into the
/// request extensions. Carries the principal id and scopes but NOT the secret,
/// so downstream handlers can attribute / authorize without re-exposing it.
#[derive(Debug, Clone)]
pub struct A2aIdentity {
    pub id: String,
    pub scopes: Vec<String>,
}

/// Build the identity for an already-authenticated direct or relay transport.
///
/// Callers must perform the transport-specific node and target ACL checks
/// before constructing this identity. The global grant prevents the standard
/// HTTP authorization layer from rejecting the same request a second time.
pub(crate) fn transport_authenticated_identity(id: String) -> A2aIdentity {
    A2aIdentity {
        id,
        scopes: vec!["*".to_owned()],
    }
}

/// Return whether a caller may dispatch one standard inbound A2A method.
///
/// Supported grants are the global `*`, `a2a:method:<Method>`, and action
/// scopes: `a2a:invoke:<agent>`, `a2a:read:<resource>`,
/// `a2a:cancel:<taskId>`, and `a2a:push:<taskId>`. Wildcard targets are
/// accepted by the shared relay scope matcher. Dev/no-auth and the gateway
/// operator remain unrestricted.
pub(crate) fn method_allowed(
    identity: Option<&A2aIdentity>,
    method: &str,
    params: &serde_json::Value,
) -> bool {
    method_allowed_with_default(identity, method, params, None)
}

/// Return whether a caller may dispatch an A2A method after resolving the
/// server's default agent for requests that omit `metadata.agentId`.
pub(crate) fn method_allowed_with_default(
    identity: Option<&A2aIdentity>,
    method: &str,
    params: &serde_json::Value,
    default_agent_id: Option<&str>,
) -> bool {
    let Some(identity) = identity else {
        return true;
    };
    if identity.id == "gateway-auth"
        || crate::a2a::relay::scope_allows(&identity.scopes, "a2a", "method", method)
    {
        return true;
    }

    let task_id = || {
        params
            .get("id")
            .or_else(|| params.get("taskId"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("*")
    };
    match method {
        "SendMessage" | "SendStreamingMessage" => {
            let target = params
                .get("metadata")
                .and_then(|metadata| metadata.get("agentId"))
                .and_then(serde_json::Value::as_str)
                .or(default_agent_id)
                .unwrap_or("*");
            crate::a2a::relay::scope_allows(&identity.scopes, "a2a", "invoke", target)
        }
        "GetExtendedAgentCard" => {
            crate::a2a::relay::scope_allows(&identity.scopes, "a2a", "read", "agent-card")
        }
        "GetTask" | "SubscribeToTask" => {
            crate::a2a::relay::scope_allows(&identity.scopes, "a2a", "read", task_id())
        }
        "ListTasks" => crate::a2a::relay::scope_allows(&identity.scopes, "a2a", "read", "tasks"),
        "CancelTask" => {
            crate::a2a::relay::scope_allows(&identity.scopes, "a2a", "cancel", task_id())
        }
        "CreateTaskPushNotificationConfig"
        | "GetTaskPushNotificationConfig"
        | "ListTaskPushNotificationConfigs"
        | "DeleteTaskPushNotificationConfig" => {
            crate::a2a::relay::scope_allows(&identity.scopes, "a2a", "push", task_id())
        }
        _ => false,
    }
}

/// Snapshot of accepted A2A credentials, materialised once per request.
struct Accepted {
    /// `gateway.auth.token` — the gateway operator master key. It matches
    /// unconditionally to the privileged `gateway-auth` identity, even when its
    /// value ALSO appears as an a2a client secret: the operator always wins, so
    /// the master token's privilege never depends on config overlap.
    operator_token: Option<String>,
    principals: Vec<A2aPrincipal>,
}

impl Accepted {
    fn is_empty(&self) -> bool {
        self.operator_token.is_none() && self.principals.is_empty()
    }

    /// Resolve a presented secret to its identity. The operator master key is
    /// checked first and always resolves to `gateway-auth` (admin). Otherwise
    /// we scan ALL principals with no early break, so timing doesn't leak how
    /// many credentials exist or where a match landed.
    fn match_secret(&self, presented: &str) -> Option<A2aIdentity> {
        if let Some(t) = &self.operator_token
            && constant_time_eq(t, presented)
        {
            return Some(A2aIdentity {
                id: "gateway-auth".to_owned(),
                scopes: Vec::new(),
            });
        }
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
/// are resolved at startup; `gateway.auth.token` is kept separate as the
/// operator master key so it always authenticates as `gateway-auth`.
async fn collect_accepted(state: &AppState) -> Accepted {
    let gw = state.live.gateway.read().await;
    Accepted {
        operator_token: gw.auth_token.clone(),
        principals: gw.a2a_principals.clone(),
    }
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

    /// Build an `Accepted` from `(id, secret)` pairs (no operator token).
    fn accepted(pairs: &[(&str, &str)]) -> Accepted {
        Accepted {
            operator_token: None,
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
        // An operator token alone keeps the endpoint guarded.
        assert!(
            !Accepted {
                operator_token: Some("t".to_owned()),
                principals: vec![],
            }
            .is_empty()
        );
    }

    #[test]
    fn operator_token_always_wins_even_on_secret_collision() {
        // gateway.auth.token shares its value with the "alice" client secret.
        // Presenting it must resolve to the privileged gateway-auth admin, not
        // to the colliding scoped principal — option A, deterministic.
        let a = Accepted {
            operator_token: Some("master".to_owned()),
            principals: vec![A2aPrincipal {
                id: "alice".to_owned(),
                secret: "master".to_owned(),
                scopes: Vec::new(),
            }],
        };
        assert_eq!(a.match_secret("master").unwrap().id, "gateway-auth");
        // A distinct client secret still resolves to its own principal.
        let a2 = Accepted {
            operator_token: Some("master".to_owned()),
            principals: vec![A2aPrincipal {
                id: "alice".to_owned(),
                secret: "sk-alice".to_owned(),
                scopes: Vec::new(),
            }],
        };
        assert_eq!(a2.match_secret("sk-alice").unwrap().id, "alice");
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
    fn standard_method_scopes_are_enforced() {
        let invoke = A2aIdentity {
            id: "partner".to_owned(),
            scopes: vec!["a2a:invoke:main".to_owned()],
        };
        let send = serde_json::json!({"metadata": {"agentId": "main"}});
        assert!(method_allowed(Some(&invoke), "SendMessage", &send));
        assert!(!method_allowed(
            Some(&invoke),
            "SendMessage",
            &serde_json::json!({"metadata": {"agentId": "other"}})
        ));
        assert!(!method_allowed(
            Some(&invoke),
            "CancelTask",
            &serde_json::json!({"id": "task-1"})
        ));

        let task = A2aIdentity {
            id: "partner".to_owned(),
            scopes: vec!["a2a:read:task-1".to_owned(), "a2a:cancel:task-1".to_owned()],
        };
        assert!(method_allowed(
            Some(&task),
            "GetTask",
            &serde_json::json!({"id": "task-1"})
        ));
        assert!(method_allowed(
            Some(&task),
            "CancelTask",
            &serde_json::json!({"id": "task-1"})
        ));
        assert!(!method_allowed(
            Some(&task),
            "GetTask",
            &serde_json::json!({"id": "task-2"})
        ));
    }

    #[test]
    fn method_and_global_wildcards_are_explicit() {
        let method = A2aIdentity {
            id: "partner".to_owned(),
            scopes: vec!["a2a:method:GetExtendedAgentCard".to_owned()],
        };
        assert!(method_allowed(
            Some(&method),
            "GetExtendedAgentCard",
            &serde_json::json!({})
        ));
        assert!(!method_allowed(
            Some(&method),
            "ListTasks",
            &serde_json::json!({})
        ));

        let all = A2aIdentity {
            id: "legacy".to_owned(),
            scopes: vec!["*".to_owned()],
        };
        assert!(method_allowed(
            Some(&all),
            "ListTasks",
            &serde_json::json!({})
        ));
    }

    #[test]
    fn default_agent_scope_is_resolved_before_authorization() {
        let invoke = A2aIdentity {
            id: "partner".to_owned(),
            scopes: vec!["a2a:invoke:main".to_owned()],
        };
        let params = serde_json::json!({"message": {"parts": []}});
        assert!(method_allowed_with_default(
            Some(&invoke),
            "SendMessage",
            &params,
            Some("main")
        ));
        assert!(!method_allowed_with_default(
            Some(&invoke),
            "SendMessage",
            &params,
            Some("other")
        ));
    }

    #[test]
    fn transport_authenticated_identity_has_global_grant() {
        let identity = transport_authenticated_identity("node:peer-a".to_owned());
        assert!(method_allowed(
            Some(&identity),
            "SendMessage",
            &serde_json::json!({"metadata": {"agentId": "main"}})
        ));
        assert!(method_allowed(
            Some(&identity),
            "GetTask",
            &serde_json::json!({"id": "task-1"})
        ));
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
