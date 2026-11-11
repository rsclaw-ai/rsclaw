//! Who a gateway request acts for.
//!
//! The gateway inserts a [`Principal`] into the extensions of every
//! authenticated request, so a route can check scopes and accounts with
//! `Option<axum::Extension<Principal>>`. Scoped tokens and the sessions
//! that carry them live in the runtime's `access` and `e2e` modules.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    /// The gateway token, or a gateway that runs without one.
    Admin,
    /// A scoped access token.
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
}
