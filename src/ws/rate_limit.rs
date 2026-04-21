//! Simple token-bucket rate limiter for WS write operations.

use std::time::{Duration, Instant};

/// Per-connection token bucket rate limiter.
///
/// Write methods (sessions.send, chat.send, config.set, etc.) consume one
/// token per call.  Read methods (health, agents.list, etc.) are free.
/// Tokens refill at a fixed rate.
pub struct RateLimiter {
    tokens: u32,
    max_tokens: u32,
    last_refill: Instant,
    refill_interval: Duration,
}

impl RateLimiter {
    /// Create a new rate limiter.
    ///
    /// `max_tokens` is the burst capacity.  `refill_interval` is how often
    /// the full bucket is restored.  For example, 30 tokens with a 60-second
    /// refill means 30 write requests per minute.
    pub fn new(max_tokens: u32, refill_interval: Duration) -> Self {
        Self {
            tokens: max_tokens,
            max_tokens,
            last_refill: Instant::now(),
            refill_interval,
        }
    }

    /// Default limiter: 30 writes per minute.
    pub fn default_write_limiter() -> Self {
        Self::new(30, Duration::from_secs(60))
    }

    /// Try to consume one token.  Returns `true` if allowed, `false` if
    /// rate-limited.
    pub fn check(&mut self) -> bool {
        self.refill();
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self) {
        let elapsed = self.last_refill.elapsed();
        if elapsed >= self.refill_interval {
            self.tokens = self.max_tokens;
            self.last_refill = Instant::now();
        }
    }

    /// Returns true if the given method is a write operation that should
    /// be rate-limited.
    pub fn is_write_method(method: &str) -> bool {
        matches!(
            method,
            "sessions.send"
                | "sessions.create"
                | "sessions.patch"
                | "sessions.compact"
                | "sessions.reset"
                | "sessions.delete"
                | "chat.send"
                | "chat.abort"
                | "agents.create"
                | "agents.update"
                | "agents.delete"
                | "agent.send"
                | "config.set"
                | "config.patch"
                | "config.apply"
                | "cron.add"
                | "cron.remove"
                | "cron.run"
                | "cron.update"
                | "cron.delete"
                | "memory.store"
                | "exec.approval.set"
                | "exec.approval.resolve"
                | "system.shutdown"
                | "system.update.run"
                | "node.pair.request"
                | "node.pair.approve"
                | "node.pair.reject"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max() {
        let mut rl = RateLimiter::new(3, Duration::from_secs(60));
        assert!(rl.check());
        assert!(rl.check());
        assert!(rl.check());
        assert!(!rl.check());
    }

    #[test]
    fn write_methods_classified() {
        assert!(RateLimiter::is_write_method("sessions.send"));
        assert!(RateLimiter::is_write_method("chat.send"));
        assert!(RateLimiter::is_write_method("config.set"));
        assert!(!RateLimiter::is_write_method("health"));
        assert!(!RateLimiter::is_write_method("agents.list"));
        assert!(!RateLimiter::is_write_method("models.list"));
    }
}
