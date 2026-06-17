//! Built-in `--template <name>` presets for `/watch`.
//!
//! The open host intentionally ships no business-specific templates. Product
//! integrations that need schema-aware SSE parsing should provide that logic
//! from their plugin instead of embedding it in rsclaw-watch.

use anyhow::{Result, anyhow};

#[derive(Debug)]
pub struct Template {
    /// Default event-type filter (`!heartbeat` -> drop heartbeats, etc).
    /// Applied only when the user did not pass `--event` themselves.
    pub event_filter: Option<&'static str>,
    /// Default jq expression. Applied only when the user did not pass
    /// `--jq` themselves. Must produce strings (one per output value).
    pub jq: Option<&'static str>,
}

/// Look up a built-in watch template by name.
pub fn lookup(name: &str) -> Result<&'static Template> {
    Err(anyhow!(
        "unknown template `{name}` — no built-in watch templates are installed"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_unknown_template_errors() {
        let err = lookup("does-not-exist").unwrap_err();
        assert!(err.to_string().contains("unknown template"));
    }
}
