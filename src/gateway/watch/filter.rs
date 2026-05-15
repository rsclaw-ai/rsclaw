//! Display pipeline: event-type filter → jq formatter → grep regex.
//!
//! Each stage either drops the event or passes through a list of
//! display strings. Most events produce 0 or 1 string; `--jq` with
//! array expansion (e.g. `.codes[]`) can produce many. `apply()`
//! returns `Vec<String>`; each entry is treated by the caller as a
//! separate event for rate limiting and chat delivery.

use anyhow::Result;
use regex::Regex;

use crate::gateway::watch::jq::CompiledJq;
use crate::gateway::watch::parser::EventFilter;
use crate::gateway::watch::source::EventRecord;

pub struct Filter {
    event_filter: Option<EventFilter>,
    jq: Option<CompiledJq>,
    grep: Option<Regex>,
}

impl Filter {
    /// Construct from parsed spec values. `jq` is compiled here (so
    /// syntax errors surface at watch start, not on the first event);
    /// `grep` likewise validates the regex up front. Either being
    /// `None` is fine — empty stages pass everything through.
    pub fn from_spec(
        grep: Option<&str>,
        jq: Option<&str>,
        event_filter: Option<EventFilter>,
    ) -> Result<Self> {
        let grep = match grep {
            Some(pat) => Some(Regex::new(pat)?),
            None => None,
        };
        let jq = match jq {
            Some(expr) => Some(CompiledJq::compile(expr)?),
            None => None,
        };
        Ok(Self {
            event_filter,
            jq,
            grep,
        })
    }

    /// Apply the full pipeline to one event. Returns the display
    /// strings that should be delivered to chat — empty list means
    /// the event was filtered out at some stage.
    pub fn apply(&self, ev: &EventRecord) -> Vec<String> {
        // Stage 1: event-type filter — fastest, runs against the
        // event's `event` field directly so heartbeats etc. never
        // reach the jq interpreter.
        if let Some(ef) = &self.event_filter
            && !ef.accepts(&ev.event)
        {
            return Vec::new();
        }

        // Stage 2: jq (or default formatter). jq runs against a
        // structured JSON view of the event so users can do
        // `select(.event == "hit") | .data.code`. When no jq is
        // configured, fall back to the default `[event] data` line.
        let pre_grep: Vec<String> = match &self.jq {
            Some(jq) => {
                let view = event_as_json(ev);
                jq.run(&view)
            }
            None => vec![default_display(ev)],
        };

        // Stage 3: grep — applied to the post-jq output so the regex
        // sees what the user would actually see. Drops non-matching
        // outputs; keeps the rest.
        match &self.grep {
            Some(re) => pre_grep.into_iter().filter(|s| re.is_match(s)).collect(),
            None => pre_grep,
        }
    }
}

/// Build the JSON view that's passed to jq. Includes the event name,
/// parsed data, optional id, and millisecond timestamp so users can
/// write expressions like `select(.event == "hit") | .data.code`.
fn event_as_json(ev: &EventRecord) -> serde_json::Value {
    let mut obj = serde_json::Map::with_capacity(4);
    obj.insert("event".into(), serde_json::Value::String(ev.event.clone()));
    obj.insert("data".into(), ev.data.clone());
    if let Some(id) = &ev.event_id {
        obj.insert("id".into(), serde_json::Value::String(id.clone()));
    }
    obj.insert(
        "ts_ms".into(),
        serde_json::Value::Number(ev.ts_ms.into()),
    );
    serde_json::Value::Object(obj)
}

/// Default display when no `--jq` is set. Matches the pre-refactor
/// behavior: line events show the raw line; everything else shows
/// `[<event_type>] <data-as-string>`.
fn default_display(ev: &EventRecord) -> String {
    if ev.event == "line" {
        return ev.raw.clone().unwrap_or_default();
    }
    let data_str = match &ev.data {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    format!("[{}] {}", ev.event, data_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev_line(s: &str) -> EventRecord {
        EventRecord {
            event: "line".into(),
            data: serde_json::Value::String(s.into()),
            raw: Some(s.into()),
            event_id: None,
            ts_ms: 0,
        }
    }

    fn ev_sse(event_type: &str, data: serde_json::Value) -> EventRecord {
        EventRecord {
            event: event_type.into(),
            data,
            raw: None,
            event_id: None,
            ts_ms: 0,
        }
    }

    #[test]
    fn none_passes_everything_through() {
        let f = Filter::from_spec(None, None, None).unwrap();
        let out = f.apply(&ev_line("hello"));
        assert_eq!(out, vec!["hello".to_owned()]);
    }

    #[test]
    fn grep_matches_on_raw() {
        let f = Filter::from_spec(Some("ERR"), None, None).unwrap();
        assert_eq!(f.apply(&ev_line("foo ERR bar")), vec!["foo ERR bar".to_owned()]);
        assert!(f.apply(&ev_line("normal line")).is_empty());
    }

    #[test]
    fn grep_invalid_regex_errors_at_construction() {
        assert!(Filter::from_spec(Some("[unclosed"), None, None).is_err());
    }

    #[test]
    fn sse_event_default_display() {
        let f = Filter::from_spec(None, None, None).unwrap();
        let ev = ev_sse("hit", json!({"code": "600192"}));
        let out = f.apply(&ev);
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("[hit]"));
        assert!(out[0].contains("600192"));
    }

    #[test]
    fn event_filter_drops_denied() {
        let ef = Some(EventFilter::Deny(vec!["heartbeat".into()]));
        let f = Filter::from_spec(None, None, ef).unwrap();
        assert!(f.apply(&ev_sse("heartbeat", json!({}))).is_empty());
        assert_eq!(f.apply(&ev_sse("hit", json!({"x": 1}))).len(), 1);
    }

    #[test]
    fn event_filter_keeps_allowed() {
        let ef = Some(EventFilter::Allow(vec!["hit".into()]));
        let f = Filter::from_spec(None, None, ef).unwrap();
        assert_eq!(f.apply(&ev_sse("hit", json!({}))).len(), 1);
        assert!(f.apply(&ev_sse("snapshot", json!({}))).is_empty());
    }

    #[test]
    fn jq_expands_array() {
        let f = Filter::from_spec(
            None,
            Some(r#".data.codes[] | "\(.code) \(.name)""#),
            None,
        )
        .unwrap();
        let ev = ev_sse(
            "snapshot",
            json!({
                "codes": [
                    {"code": "601225", "name": "陕西煤业"},
                    {"code": "002327", "name": "富安娜"}
                ]
            }),
        );
        let out = f.apply(&ev);
        assert_eq!(
            out,
            vec!["601225 陕西煤业".to_owned(), "002327 富安娜".to_owned()]
        );
    }

    #[test]
    fn jq_select_drops_non_matching() {
        let f =
            Filter::from_spec(None, Some(r#"select(.event == "hit") | .data.code"#), None)
                .unwrap();
        assert!(f.apply(&ev_sse("heartbeat", json!({}))).is_empty());
        assert_eq!(
            f.apply(&ev_sse("hit", json!({"code": "600192"}))),
            vec!["600192".to_owned()]
        );
    }

    #[test]
    fn full_pipeline_event_then_jq_then_grep() {
        // event filter blocks heartbeat → jq formats hit → grep keeps
        // only rally-tagged outputs.
        let f = Filter::from_spec(
            Some("quick_rally"),
            Some(r#".data | "\(.code) \(.name) [\(.filter)]""#),
            Some(EventFilter::Deny(vec!["heartbeat".into()])),
        )
        .unwrap();
        assert!(f.apply(&ev_sse("heartbeat", json!({}))).is_empty());
        let rally = ev_sse(
            "hit",
            json!({"code": "600192", "name": "长城电工", "filter": "quick_rally"}),
        );
        assert_eq!(
            f.apply(&rally),
            vec!["600192 长城电工 [quick_rally]".to_owned()]
        );
        let goldcross = ev_sse(
            "hit",
            json!({"code": "601225", "name": "陕西煤业", "filter": "quick_goldcross"}),
        );
        assert!(
            f.apply(&goldcross).is_empty(),
            "grep should drop non-rally outputs"
        );
    }
}
