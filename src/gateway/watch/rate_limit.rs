//! Per-window event batching.
//!
//! See spec §"RateLimiter". Default: 1 event per 2000 ms window; subsequent
//! events buffered until next tick, then emitted as a single "N more events"
//! batch message.

#[derive(Debug, Clone, PartialEq)]
pub enum DeliveryMsg {
    Single(String),
    /// N>=1 events were dropped from chat (rate-limited); show the last one.
    Batch { last: String, dropped: usize },
}

#[derive(Debug)]
pub struct RateLimiter {
    window_ms: u64,
    max_per_window: usize,  // 0 means unlimited
    buffer: Vec<String>,
    last_emit_ms: Option<u64>,  // None = no emit yet (window is open)
    last_seen_ms: u64,
}

impl RateLimiter {
    /// `rate_ms` = 0 disables the limiter (every call → Single).
    pub fn new(rate_ms: u64) -> Self {
        Self {
            window_ms: rate_ms,
            max_per_window: if rate_ms == 0 { 0 } else { 1 },
            buffer: Vec::new(),
            last_emit_ms: None,
            last_seen_ms: 0,
        }
    }

    /// Try to admit a new message at `now_ms`. Returns `Some(msg)` if it
    /// should be delivered immediately, `None` if it was buffered.
    pub fn admit(&mut self, msg: String, now_ms: u64) -> Option<DeliveryMsg> {
        self.last_seen_ms = now_ms;
        if self.max_per_window == 0 {
            return Some(DeliveryMsg::Single(msg));
        }
        let window_open = self
            .last_emit_ms
            .map(|last| now_ms.saturating_sub(last) >= self.window_ms)
            .unwrap_or(true);
        if window_open {
            self.last_emit_ms = Some(now_ms);
            Some(DeliveryMsg::Single(msg))
        } else {
            self.buffer.push(msg);
            None
        }
    }

    /// Drive the limiter from a periodic tick. If anything is buffered and
    /// the window has now closed, emit a batch message and clear the buffer.
    pub fn flush_pending(&mut self, now_ms: u64) -> Option<DeliveryMsg> {
        if self.max_per_window == 0 || self.buffer.is_empty() {
            return None;
        }
        let window_closed = self
            .last_emit_ms
            .map(|last| now_ms.saturating_sub(last) >= self.window_ms)
            .unwrap_or(true);
        if !window_closed {
            return None;
        }
        let last = self.buffer.pop().expect("buffer not empty");
        let dropped = self.buffer.len();
        self.buffer.clear();
        self.last_emit_ms = Some(now_ms);
        Some(DeliveryMsg::Batch { last, dropped })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_message_in_window_is_single() {
        let mut r = RateLimiter::new(2000);
        assert_eq!(r.admit("a".into(), 0), Some(DeliveryMsg::Single("a".into())));
    }

    #[test]
    fn subsequent_in_window_buffered() {
        let mut r = RateLimiter::new(2000);
        assert!(r.admit("a".into(), 0).is_some());
        assert!(r.admit("b".into(), 500).is_none());
        assert!(r.admit("c".into(), 1500).is_none());
    }

    #[test]
    fn flush_after_window_emits_batch() {
        let mut r = RateLimiter::new(2000);
        r.admit("a".into(), 0);          // → Single("a")
        r.admit("b".into(), 500);        // → None (buffered)
        r.admit("c".into(), 1500);       // → None (buffered)
        let flushed = r.flush_pending(2000);
        // Last buffered = "c", dropped = 1 (just "b").
        assert_eq!(flushed, Some(DeliveryMsg::Batch { last: "c".into(), dropped: 1 }));
    }

    #[test]
    fn flush_with_empty_buffer_returns_none() {
        let mut r = RateLimiter::new(2000);
        assert_eq!(r.flush_pending(5000), None);
    }

    #[test]
    fn flush_inside_window_returns_none() {
        let mut r = RateLimiter::new(2000);
        r.admit("a".into(), 0);
        r.admit("b".into(), 500);
        // Still inside the 2s window.
        assert_eq!(r.flush_pending(1500), None);
    }

    #[test]
    fn rate_zero_disables_limit() {
        let mut r = RateLimiter::new(0);
        assert_eq!(r.admit("a".into(), 0), Some(DeliveryMsg::Single("a".into())));
        assert_eq!(r.admit("b".into(), 1), Some(DeliveryMsg::Single("b".into())));
        assert_eq!(r.admit("c".into(), 2), Some(DeliveryMsg::Single("c".into())));
        assert_eq!(r.flush_pending(3), None);
    }

    #[test]
    fn second_window_emits_single_again() {
        let mut r = RateLimiter::new(2000);
        assert!(matches!(r.admit("a".into(), 0), Some(DeliveryMsg::Single(_))));
        r.admit("b".into(), 500);
        let _ = r.flush_pending(2000);
        // Flush emitted at t=2000; next window opens at t=4000.
        assert!(r.admit("c".into(), 2100).is_none(), "still in window");
        assert!(
            matches!(r.admit("d".into(), 4001), Some(DeliveryMsg::Single(_))),
            "next window should be open"
        );
    }
}
