//! Notification sink trait — lifted to rsclaw-types (crate-split) so lower
//! crates (channel) can implement it without depending on cap. Re-exported
//! here for backward-compat at crate::cap::notification::.
pub use rsclaw_types::{Notification, NotificationPriority, NotificationSink};
