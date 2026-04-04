//! OpenClaw WebSocket Gateway Protocol v3.
//!
//! Handles server-initiated handshake, device pairing, bidirectional
//! req/res RPC, and push events (tick, presence, session messages).

pub mod conn;
pub mod dispatch;
pub mod handshake;
pub mod methods;
pub mod rate_limit;
pub mod tick;
pub mod types;

pub use conn::{ConnHandle, ConnRegistry};
pub use handshake::{DeviceStore, root_handler, ws_handler};
pub use types::{ErrorShape, EventFrame};
