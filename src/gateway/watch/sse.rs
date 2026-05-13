//! SSE wire parser + single-connection runner.
//!
//! See spec §"SseSource 实现策略". Heartbeat / ${VAR} substitution are added in
//! Tasks 12/13/14. Reconnect + Last-Event-ID + `retry:` are wired in Task 11.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use super::source::{EventRecord, SseSource};

const ACCEPT: &str = "text/event-stream";
const CACHE_CONTROL: &str = "no-cache";
const ACCEPT_ENCODING: &str = "identity"; // forbid gzip — buffering kills SSE

/// Outcome of one connection attempt.
#[derive(Debug)]
pub(super) enum SseOutcome {
    /// Caller requested stop.
    Stopped,
    /// Server returned a fatal HTTP status (4xx). Don't retry.
    Fatal(String),
    /// Connection ended cleanly or transient error — caller may retry.
    Disconnect(String),
}

async fn run_sse_single_tracking(
    src: &SseSource,
    tx: &mpsc::Sender<EventRecord>,
    stop: &mut oneshot::Receiver<()>,
    out_last_id: &mut Option<String>,
    out_backoff_ms: &mut u64,
) -> SseOutcome {
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => return SseOutcome::Disconnect(format!("client build: {e}")),
    };

    let mut req = client
        .get(&src.url)
        .header(reqwest::header::ACCEPT, ACCEPT)
        .header(reqwest::header::CACHE_CONTROL, CACHE_CONTROL)
        .header(reqwest::header::ACCEPT_ENCODING, ACCEPT_ENCODING);
    for (name, value) in &src.headers {
        req = req.header(name.as_str(), value.as_str());
    }
    // Resume header from prior connection's last seen id.
    if let Some(id) = out_last_id.as_deref() {
        req = req.header("Last-Event-ID", id);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return SseOutcome::Disconnect(format!("connect: {e}")),
    };
    let status = resp.status();
    if matches!(status.as_u16(), 401 | 403 | 404) {
        return SseOutcome::Fatal(format!("server returned {}", status.as_u16()));
    }
    if !status.is_success() {
        return SseOutcome::Disconnect(format!("server returned {}", status.as_u16()));
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct.contains("text/event-stream") {
        return SseOutcome::Fatal(format!("non-SSE content type: {ct}"));
    }

    // First success of this attempt → reset the backoff.
    *out_backoff_ms = 2000;

    let mut stream = resp.bytes_stream();
    let mut parser = SseParser::default();
    let mut last_byte = tokio::time::Instant::now();
    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = &mut *stop => return SseOutcome::Stopped,
            _ = watchdog.tick() => {
                if last_byte.elapsed() > Duration::from_secs(90) {
                    let _ = tx
                        .send(EventRecord::lifecycle(
                            "_timeout",
                            serde_json::json!({"msg": "no bytes for 90s"}),
                            now_ms(),
                        ))
                        .await;
                    return SseOutcome::Disconnect("heartbeat_timeout".into());
                }
            }
            chunk = stream.next() => match chunk {
                Some(Ok(bytes)) => {
                    last_byte = tokio::time::Instant::now();
                    for ev in parser.feed(&bytes) {
                        if let Some(id) = &ev.event_id {
                            *out_last_id = Some(id.clone());
                        }
                        if tx.send(ev).await.is_err() {
                            return SseOutcome::Stopped;
                        }
                    }
                    if let Some(rt) = parser.last_retry_ms.take() {
                        *out_backoff_ms = rt.min(30_000);
                    }
                }
                Some(Err(e)) => return SseOutcome::Disconnect(format!("stream: {e}")),
                None => return SseOutcome::Disconnect("server_closed".into()),
            }
        }
    }
}

/// Incremental SSE wire-format parser.
///
/// Spec rules implemented:
/// - lines separated by `\n` or `\r\n`
/// - `event: <type>` → set current event type (reset on each block)
/// - `data: <text>` → append to data buffer, multiple data: lines joined by `\n`
/// - `id: <id>` → record on the event
/// - `retry: <ms>` → currently ignored at this layer (Task 11 plumbs it through)
/// - lines starting with `:` are comments (ignored)
/// - blank line → flush the current event
#[derive(Default)]
pub(super) struct SseParser {
    leftover: Vec<u8>,
    event_type: Option<String>,
    data_lines: Vec<String>,
    last_id_seen: Option<String>,
    pub last_retry_ms: Option<u64>,
}

impl SseParser {
    pub fn feed(&mut self, bytes: &Bytes) -> Vec<EventRecord> {
        self.leftover.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(idx) = self.leftover.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.leftover.drain(..=idx).collect();
            // Drop trailing \r\n or \n.
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(ev) = self.consume_line(&line) {
                out.push(ev);
            }
        }
        out
    }

    fn consume_line(&mut self, line: &str) -> Option<EventRecord> {
        if line.is_empty() {
            return self.flush();
        }
        if let Some(stripped) = line.strip_prefix(':') {
            // Comment. quick_stream.py compatibility: treat as silent.
            let _ = stripped;
            return None;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            self.event_type = Some(rest.trim().to_owned());
            return None;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
            return None;
        }
        if let Some(rest) = line.strip_prefix("id:") {
            let id = rest.trim().to_owned();
            if !id.is_empty() {
                self.last_id_seen = Some(id);
            }
            return None;
        }
        if let Some(rest) = line.strip_prefix("retry:") {
            if let Ok(ms) = rest.trim().parse::<u64>() {
                self.last_retry_ms = Some(ms);
            }
            return None;
        }
        // Unknown field — SSE spec says ignore.
        None
    }

    fn flush(&mut self) -> Option<EventRecord> {
        if self.data_lines.is_empty() {
            // Reset event_type even on no-data flush.
            self.event_type = None;
            return None;
        }
        let joined = self.data_lines.join("\n");
        let parsed = serde_json::from_str::<serde_json::Value>(&joined)
            .unwrap_or_else(|e| serde_json::json!({"_parse_error": e.to_string(), "_raw": joined}));
        let ev = EventRecord {
            event: self.event_type.take().unwrap_or_else(|| "message".into()),
            data: parsed,
            raw: None,
            event_id: self.last_id_seen.clone(),
            ts_ms: now_ms(),
        };
        self.data_lines.clear();
        Some(ev)
    }

    pub fn last_id_seen(&self) -> Option<&str> {
        self.last_id_seen.as_deref()
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) async fn run_sse(
    src: SseSource,
    tx: mpsc::Sender<EventRecord>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut last_event_id: Option<String> = None;
    let mut backoff_ms: u64 = 2000;

    loop {
        let outcome = run_sse_single_tracking(
            &src,
            &tx,
            &mut stop,
            &mut last_event_id,
            &mut backoff_ms,
        )
        .await;

        match outcome {
            SseOutcome::Stopped => return,
            SseOutcome::Fatal(msg) => {
                let _ = tx
                    .send(EventRecord::lifecycle(
                        "_error",
                        serde_json::json!({ "fatal": true, "msg": msg }),
                        now_ms(),
                    ))
                    .await;
                return;
            }
            SseOutcome::Disconnect(_msg) => {
                // Wait `backoff_ms` (capped at 30s) before reconnecting,
                // unless a `retry:` header overrode it inside the connection.
                let delay = backoff_ms.min(30_000);
                tokio::select! {
                    _ = &mut stop => return,
                    _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                }
                // Exponential backoff with 30s cap.
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_message() {
        let mut p = SseParser::default();
        let evs = p.feed(&Bytes::from_static(b"data: {\"x\":1}\n\n"));
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event, "message");
        assert_eq!(evs[0].data, serde_json::json!({"x": 1}));
    }

    #[test]
    fn parses_event_type() {
        let mut p = SseParser::default();
        let evs = p.feed(&Bytes::from_static(b"event: hit\ndata: {\"code\":\"600519\"}\n\n"));
        assert_eq!(evs[0].event, "hit");
    }

    #[test]
    fn comments_are_ignored() {
        let mut p = SseParser::default();
        let evs = p.feed(&Bytes::from_static(b": heartbeat\ndata: {\"x\":1}\n\n"));
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn multi_data_lines_joined_with_newline() {
        let mut p = SseParser::default();
        let evs = p.feed(&Bytes::from_static(b"data: line1\ndata: line2\n\n"));
        // The two data lines join with `\n` -> "line1\nline2" which is NOT valid JSON,
        // so we expect the _parse_error / _raw fallback.
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data["_raw"], serde_json::Value::String("line1\nline2".into()));
    }

    #[test]
    fn invalid_json_falls_back_to_parse_error() {
        let mut p = SseParser::default();
        let evs = p.feed(&Bytes::from_static(b"data: not json\n\n"));
        assert!(evs[0].data.get("_parse_error").is_some());
        assert_eq!(evs[0].data["_raw"], serde_json::Value::String("not json".into()));
    }

    #[test]
    fn id_field_is_recorded() {
        let mut p = SseParser::default();
        let evs = p.feed(&Bytes::from_static(b"id: 42\ndata: {\"x\":1}\n\n"));
        assert_eq!(evs[0].event_id.as_deref(), Some("42"));
        assert_eq!(p.last_id_seen(), Some("42"));
    }

    #[test]
    fn retry_field_captured() {
        let mut p = SseParser::default();
        let _ = p.feed(&Bytes::from_static(b"retry: 4500\n\n"));
        assert_eq!(p.last_retry_ms, Some(4500));
    }

    #[test]
    fn split_chunk_boundary() {
        let mut p = SseParser::default();
        let evs1 = p.feed(&Bytes::from_static(b"data: {\"x"));
        assert!(evs1.is_empty());
        let evs2 = p.feed(&Bytes::from_static(b"\":1}\n\n"));
        assert_eq!(evs2.len(), 1);
        assert_eq!(evs2[0].data, serde_json::json!({"x": 1}));
    }
}

/// Replace every `${VAR}` occurrence in `s` with `std::env::var("VAR")`.
/// Returns `Err(VAR)` for the first unresolved or empty-valued variable.
pub(crate) fn substitute_env_vars(s: &str) -> Result<String> {
    let re = regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").unwrap();
    let mut last = 0usize;
    let mut out = String::with_capacity(s.len());
    for cap in re.captures_iter(s) {
        let m = cap.get(0).unwrap();
        let name = cap.get(1).unwrap().as_str();
        out.push_str(&s[last..m.start()]);
        let val = std::env::var(name).unwrap_or_default();
        if val.is_empty() {
            return Err(anyhow!("{name}"));
        }
        out.push_str(&val);
        last = m.end();
    }
    out.push_str(&s[last..]);
    Ok(out)
}

#[cfg(test)]
mod subst_tests {
    use super::*;
    #[test]
    fn passes_through_without_vars() {
        assert_eq!(substitute_env_vars("plain").unwrap(), "plain");
    }
    #[test]
    fn substitutes_set_var() {
        // Use a unique name so other tests don't race.
        unsafe { std::env::set_var("WATCH_TEST_TOKEN", "xyz") };
        assert_eq!(
            substitute_env_vars("Bearer ${WATCH_TEST_TOKEN}").unwrap(),
            "Bearer xyz"
        );
        unsafe { std::env::remove_var("WATCH_TEST_TOKEN") };
    }
    #[test]
    fn errors_on_missing_var() {
        unsafe { std::env::remove_var("WATCH_TEST_MISSING") };
        let err = substitute_env_vars("Bearer ${WATCH_TEST_MISSING}").unwrap_err();
        assert!(err.to_string().contains("WATCH_TEST_MISSING"));
    }
}
