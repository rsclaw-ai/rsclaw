//! Google A2A Protocol v1.0 implementation.
//!
//! Spec: https://a2a-protocol.org/latest/specification/

pub mod auth;
pub mod errors;
pub mod files;
pub mod peer;
pub mod push;
pub mod relay;
pub mod relay_identity;
pub mod server;
pub mod store;
pub mod streaming;
pub mod stun;
pub mod version;

// event + types lifted to rsclaw-a2a-types (crate-split step-12 P1);
// re-exported so crate::a2a::{event,types}:: paths
// (server/gateway/a2a-internal) keep resolving.
pub use rsclaw_a2a_types::{client, event, types, types::*};
