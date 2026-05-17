//! Google A2A Protocol v1.0 implementation.
//!
//! Spec: https://a2a-protocol.org/latest/specification/

pub mod client;
pub mod event;
pub mod server;
pub mod streaming;
pub mod types;

pub use types::*;
