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
