//! Extension points for crates linked into a custom engine binary.
//!
//! A custom binary is a thin `main` that registers its extensions and then
//! calls [`crate::run`], like the stock `src/main.rs`. The stock binary
//! registers nothing, so the gateway behaves exactly as before.

use std::sync::{Arc, RwLock};

use axum::Router;

type RoutesFactory = Arc<dyn Fn() -> Router + Send + Sync>;

static API_ROUTES: RwLock<Vec<RoutesFactory>> = RwLock::new(Vec::new());

/// Register extra gateway routes under `/api/v1`.
///
/// `factory` runs each time the gateway router is built, after the config
/// and data directory are resolved, so it may read them. Its routes sit
/// behind the same auth, origin check, rate limit and host guard as the
/// built-in API. Paths must not overlap built-in routes (axum refuses
/// overlapping routes when the router is built).
pub fn register_api_routes(factory: impl Fn() -> Router + Send + Sync + 'static) {
    let mut routes = API_ROUTES
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    routes.push(Arc::new(factory));
}

/// The routers of every registered extension, freshly built.
pub(crate) fn api_routes() -> Vec<Router> {
    let routes = API_ROUTES
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    routes.iter().map(|factory| factory()).collect()
}

/// An API area that scoped access tokens may be minted for (see
/// [`crate::access`]). A scope is `<name>:<action>`, and a session opened
/// with such a token reaches only `/api/v1/<name>/...`.
#[derive(Debug, Clone, Copy)]
pub struct ScopedArea {
    pub name: &'static str,
    pub actions: &'static [&'static str],
    /// Refuse plaintext requests to `/api/v1/<name>/...`: they must arrive
    /// inside an end-to-end session (see [`crate::e2e`]), whoever sends them.
    pub e2e_only: bool,
}

/// Built-in API areas an extension may not claim.
const RESERVED_AREAS: &[&str] = &["tokens", "e2e", "health"];

static SCOPED_AREAS: RwLock<Vec<ScopedArea>> = RwLock::new(Vec::new());

/// Register an area for scoped tokens. Registering the same name again
/// replaces the earlier entry.
///
/// # Panics
/// If `area.name` is a built-in area (`tokens`, `e2e`, `health`) or is not a
/// lowercase identifier.
pub fn register_scoped_area(area: ScopedArea) {
    assert!(
        !RESERVED_AREAS.contains(&area.name)
            && !area.name.is_empty()
            && area
                .name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'),
        "scoped area name {:?} is reserved or malformed",
        area.name
    );
    let mut areas = SCOPED_AREAS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    areas.retain(|a| a.name != area.name);
    areas.push(area);
}

pub(crate) fn scoped_area(name: &str) -> Option<ScopedArea> {
    SCOPED_AREAS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .find(|a| a.name == name)
        .copied()
}

/// True when `path` belongs to an area that only accepts end-to-end
/// encrypted requests.
pub(crate) fn e2e_only(path: &str) -> bool {
    path.strip_prefix("/api/v1/")
        .and_then(|rest| rest.split('/').next())
        .and_then(scoped_area)
        .is_some_and(|a| a.e2e_only)
}
