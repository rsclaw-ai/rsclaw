//! Grep / jq filter pipeline.
//!
//! For v1 only Grep is implemented; Jq is a stretch goal (see Task S1).
//! Filter::apply consumes one EventRecord and returns the display string
//! (Some) or drops the event (None).

use anyhow::Result;
use regex::Regex;

use crate::gateway::watch::source::EventRecord;

#[derive(Debug)]
pub enum Filter {
    None,
    Grep(Regex),
    /// Placeholder for jq — wired into the parser already but apply() falls
    /// back to passing the event through unchanged so the rest of the pipeline
    /// is testable. Task S1 replaces this with a real interpreter.
    JqStub,
}

impl Filter {
    /// Construct from parsed spec values. `grep` validates the regex,
    /// `jq` is accepted as-is (interpreter wired up in stretch task).
    pub fn from_spec(grep: Option<&str>, jq: Option<&str>) -> Result<Self> {
        if let Some(pat) = grep {
            return Ok(Filter::Grep(Regex::new(pat)?));
        }
        if jq.is_some() {
            return Ok(Filter::JqStub);
        }
        Ok(Filter::None)
    }

    /// Returns `Some(display_text)` if the event passes; `None` to drop.
    pub fn apply(&self, ev: &EventRecord) -> Option<String> {
        match self {
            Filter::None | Filter::JqStub => Some(self.default_display(ev)),
            Filter::Grep(re) => {
                let haystack = ev.raw.clone().unwrap_or_else(|| self.default_display(ev));
                if re.is_match(&haystack) {
                    Some(haystack)
                } else {
                    None
                }
            }
        }
    }

    fn default_display(&self, ev: &EventRecord) -> String {
        // For shell/file events, just show the raw line.
        // For SSE, format as `[event_type] <data-as-string>`.
        if ev.event == "line" {
            return ev.raw.clone().unwrap_or_default();
        }
        let data_str = match &ev.data {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        format!("[{}] {}", ev.event, data_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_line(s: &str) -> EventRecord {
        EventRecord {
            event: "line".into(),
            data: serde_json::Value::String(s.into()),
            raw: Some(s.into()),
            event_id: None,
            ts_ms: 0,
        }
    }

    fn ev_sse(event: &str, data: serde_json::Value) -> EventRecord {
        EventRecord {
            event: event.into(),
            data,
            raw: None,
            event_id: None,
            ts_ms: 0,
        }
    }

    #[test]
    fn none_passes_everything_through() {
        let f = Filter::from_spec(None, None).unwrap();
        assert_eq!(f.apply(&ev_line("hello")), Some("hello".into()));
    }

    #[test]
    fn grep_matches_on_raw() {
        let f = Filter::from_spec(Some("ERR"), None).unwrap();
        assert_eq!(f.apply(&ev_line("INFO hello")), None);
        assert_eq!(f.apply(&ev_line("ERR boom")), Some("ERR boom".into()));
    }

    #[test]
    fn grep_invalid_regex_errors_at_construction() {
        assert!(Filter::from_spec(Some("[unclosed"), None).is_err());
    }

    #[test]
    fn sse_event_default_display() {
        let f = Filter::from_spec(None, None).unwrap();
        let ev = ev_sse("hit", serde_json::json!({"code": "600519"}));
        let out = f.apply(&ev).unwrap();
        assert!(out.starts_with("[hit]"), "got: {out}");
        assert!(out.contains("600519"), "got: {out}");
    }

    #[test]
    fn grep_falls_back_to_default_display_when_no_raw() {
        let f = Filter::from_spec(Some("hit"), None).unwrap();
        let ev = ev_sse("hit", serde_json::json!({"code": "x"}));
        assert!(f.apply(&ev).is_some());
    }
}
