//! Platform / target operator implementations.

pub mod browser;
pub mod iphone_mirror;
pub mod native;

pub use browser::BrowserOperator;
pub use iphone_mirror::IphoneMirrorOperator;
pub use native::NativeOperator;
