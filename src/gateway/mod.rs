//! Gateway subsystem.
//!
//! Modules:
//!   - `router`      — channel → agent routing (bindings[] + channel
//!     declarations)
//!   - `session`     — session key derivation + dmScope isolation
//!   - `hot_reload`  — config file watcher + change event broadcasting

pub mod channels;
pub mod hot_reload;
pub mod live_config;
pub mod preparse;
pub mod providers;
pub mod router;
pub mod session;
pub mod shutdown;
pub mod startup;
pub mod task_queue;

pub use hot_reload::{ConfigChange, FileWatcher};
pub use live_config::LiveConfig;
pub use router::{InboundMessage, Router};
pub use session::{
    CronSessionMode, MessageKind, SessionKeyParams, derive_session_key, resolve_identity,
};
pub use shutdown::{InflightGuard, ShutdownCoordinator};
