//! Bounded retry for outbound message HTTP sends.
//!
//! Channels consume their outbound queue serially (one `while let
//! Some(msg) = out_rx.recv()` loop per channel), so a long backoff blocks
//! every later delivery for that channel. This is delivery resilience
//! against transient transport resets (idle keep-alive RST surfacing as
//! `Connection reset by peer`, os error 54), not a rate-limit backoff —
//! keep both the attempt count and the delay small.
//!
//! Retrying a non-idempotent POST risks double delivery when the request
//! reached the server and committed but the response was lost. Callers MUST
//! embed a stable idempotency key in the request that does not change
//! between attempts (Feishu `uuid`, WeChat `client_id`, QQ `msg_id`+`msg_seq`)
//! so the server collapses the duplicate. Where no such key exists (a custom
//! user webhook), retry only buys resilience against pre-commit resets and a
//! post-commit reset may duplicate — document that on the call site.

use std::time::Duration;

use anyhow::Result;

/// Retry policy for an outbound send. Defaults are deliberately tight: one
/// retry after the first failure, sub-second backoff.
pub struct SendRetry {
    pub attempts: u32,
    pub min_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for SendRetry {
    fn default() -> Self {
        // attempts=2: first send + one retry. min/max keep the serial
        // outbound loop from stalling more than ~2s on a dead peer.
        Self {
            attempts: 2,
            min_delay_ms: 300,
            max_delay_ms: 2_000,
        }
    }
}

impl SendRetry {
    /// Backoff before `attempt` (0-based; attempt 0 never sleeps). Exponential
    /// from `min_delay_ms`, clamped to `max_delay_ms`, with up to 20% jitter to
    /// avoid synchronised retries across concurrent channels.
    fn delay(&self, attempt: u32) -> Duration {
        let factor = 1u64 << attempt.min(16);
        let base = self.min_delay_ms.saturating_mul(factor).min(self.max_delay_ms);
        let jitter = (base as f64 * 0.2 * rand::random::<f64>()) as u64;
        Duration::from_millis(base + jitter)
    }
}

/// Send an HTTP request with bounded retry on transient transport failures
/// and 5xx responses, returning the first response whose status is not 5xx.
///
/// The caller classifies what comes back: 2xx success, 4xx permanent failure,
/// or an in-body business error code. 4xx is intentionally NOT retried here —
/// a bad token or recipient will not recover and retrying just delays the real
/// error.
///
/// `make_req` is invoked once per attempt and must rebuild an equivalent
/// request each time (a `RequestBuilder` is consumed by `send`). Embed a stable
/// idempotency key inside it — see the module docs.
pub async fn send_with_retry(
    label: &str,
    retry: &SendRetry,
    make_req: impl Fn() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response> {
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..retry.attempts {
        if attempt > 0 {
            let delay = retry.delay(attempt);
            tracing::warn!(
                channel = label,
                attempt,
                delay_ms = delay.as_millis() as u64,
                "send failed, retrying"
            );
            tokio::time::sleep(delay).await;
        }

        match make_req().send().await {
            Ok(resp) if resp.status().is_server_error() => {
                // 5xx may be a transient server-side blip — retry. Drain the
                // body into the error so the final failure is diagnosable.
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                last_err = Some(anyhow::anyhow!("{label}: send failed {status}: {body}"));
            }
            Ok(resp) => return Ok(resp),
            Err(e) => {
                // Transport-level failure (connection reset, timeout, DNS,
                // TLS). The dominant cause is a stale pooled keep-alive
                // connection the peer already closed; a fresh attempt re-dials.
                last_err = Some(anyhow::Error::new(e).context(format!("{label}: send request")));
            }
        }
    }

    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("{label}: send failed after {} attempts", retry.attempts)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_is_bounded_and_monotonic_base() {
        let r = SendRetry::default();
        // attempt 0 base is min_delay; jitter keeps it within [base, 1.2*base].
        let d1 = r.delay(1);
        assert!(d1 >= Duration::from_millis(600) && d1 <= Duration::from_millis(720));
        // High attempts clamp to max_delay (+jitter), never overflow.
        let d_big = r.delay(40);
        assert!(d_big >= Duration::from_millis(2_000) && d_big <= Duration::from_millis(2_400));
    }
}
