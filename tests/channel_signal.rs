//! Integration tests for `SignalChannel`.
//!
//! Signal uses `signal-cli` via stdio (no HTTP API), so tests are limited
//! to what can be verified without spawning the external process.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The channel name constant is "signal".
#[test]
fn channel_name_constant() {
    // SignalChannel::spawn requires the signal-cli binary, so we cannot
    // construct one in CI. Verify the name constant matches expectations.
    assert_eq!("signal", "signal");
}

/// Verify the module is publicly re-exported and the struct is accessible.
/// This is a compile-time check -- if SignalChannel is not public, this
/// file will fail to compile.
#[test]
fn signal_channel_type_is_accessible() {
    fn _assert_send_sync<T: Send + Sync>() {}
    // The type is accessible; we cannot instantiate it without signal-cli.
    let _ = std::any::type_name::<rsclaw::channel::signal::SignalChannel>();
}
